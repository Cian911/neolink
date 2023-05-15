//! Attempts to subclass RtspServer
//!
//! We are now messing with gstreamer glib objects
//! expect issues

use super::{factory::*, AnyResult};
use crate::config::*;

use anyhow::{anyhow, Context};
use gstreamer::glib::{self, object_subclass, subclass::types::ObjectSubclass, MainLoop, Object};
use gstreamer_rtsp::RTSPAuthMethod;
use gstreamer_rtsp_server::{
    gio::{TlsAuthenticationMode, TlsCertificate},
    prelude::*,
    subclass::prelude::*,
    RTSPAuth, RTSPFilterResult, RTSPServer, RTSPToken, RTSP_TOKEN_MEDIA_FACTORY_ROLE,
};
use log::*;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    fs,
    sync::Arc,
};
use tokio::{
    sync::{mpsc::Sender, RwLock},
    task::JoinSet,
    time::{timeout, Duration},
};

glib::wrapper! {
    /// The wrapped RTSPServer
    pub(crate) struct NeoRtspServer(ObjectSubclass<NeoRtspServerImpl>) @extends RTSPServer;
}

impl Default for NeoRtspServer {
    fn default() -> Self {
        Self::new().unwrap()
    }
}

impl NeoRtspServer {
    pub(crate) fn new() -> AnyResult<Self> {
        gstreamer::init().context("Gstreamer failed to initialise")?;
        let factory = Object::new::<NeoRtspServer>();
        Ok(factory)
    }

    pub(crate) async fn get_sender<T: Into<String>>(
        &self,
        tag: T,
    ) -> Option<Sender<FactoryCommand>> {
        self.imp().get_sender(tag).await
    }

    pub(crate) async fn create_stream<U: Into<String>>(&self, tag: U) -> AnyResult<()> {
        self.imp().create_stream(tag).await
    }

    #[allow(dead_code)]
    pub(crate) async fn remove_stream<T: Into<String>>(&self, tag: T) -> AnyResult<()> {
        self.imp().remove_stream(tag).await
    }

    #[allow(dead_code)]
    pub(crate) async fn remove_path<T: Into<String>>(
        &self,
        tag: T,
        paths: &[&str],
    ) -> AnyResult<()> {
        self.imp().remove_path(tag, paths).await
    }

    pub(crate) async fn add_path<T: Into<String>>(
        &self,
        tag: T,
        paths: &[String],
    ) -> AnyResult<()> {
        self.imp().add_path(tag, paths).await
    }

    pub(crate) async fn add_permitted_roles<T: Into<String>, U: AsRef<str>>(
        &self,
        tag: T,
        permitted_users: &HashSet<U>,
    ) -> AnyResult<()> {
        self.imp().add_permitted_roles(tag, permitted_users).await
    }

    pub(crate) async fn run(&self, bind_addr: &str, bind_port: u16) -> AnyResult<()> {
        let server = self;
        server.set_address(bind_addr);
        server.set_service(&format!("{}", bind_port));
        // Attach server to default Glib context
        let _ = server.attach(None);
        let main_loop = Arc::new(MainLoop::new(None, false));
        // Run the Glib main loop.
        let main_loop_thread = main_loop.clone();
        let handle = tokio::task::spawn_blocking(move || {
            main_loop_thread.run();
            AnyResult::Ok(())
        });
        timeout(Duration::from_secs(5), self.imp().threads.write())
            .await
            .with_context(|| "Timeout waiting to lock Server threads")?
            .spawn(async move { handle.await? });
        timeout(Duration::from_secs(5), self.imp().main_loop.write())
            .await
            .with_context(|| "Timeout waiting to lock Server main_loop")?
            .replace(main_loop);
        Ok(())
    }

    pub(crate) async fn quit(&self) -> AnyResult<()> {
        if let Some(main_loop) = self.imp().main_loop.read().await.as_ref() {
            main_loop.quit();
        }
        Ok(())
    }

    pub(crate) async fn join(&self) -> AnyResult<()> {
        let mut threads = self.imp().threads.write().await;
        while let Some(thread) = threads.join_next().await {
            thread??;
        }
        Ok(())
    }

    pub(crate) fn set_up_tls(&self, config: &Config) {
        self.imp().set_up_tls(config)
    }

    pub(crate) fn set_up_users(&self, users: &[UserConfig]) {
        self.imp().set_up_users(users)
    }

    #[allow(dead_code)]
    /// This will return the total number of active clients
    pub(crate) fn num_clients(&self) -> usize {
        self.client_filter(None).len()
    }

    /// This will get the number of users for any of the given paths
    ///
    /// This function is not quite what we want as it only count when the media is active
    /// we want something that returns >0 when we want to build a media
    #[allow(dead_code)]
    pub(crate) fn num_path_users(&self, paths: &[&str]) -> usize {
        self.session_pool().map_or(0usize, |pool| {
            pool.filter(None).iter().fold(0usize, |acc, session| {
                acc + session
                    .filter(None)
                    .iter()
                    .fold(0usize, |acc_b, media_session| {
                        acc_b
                            + if paths.iter().any(|path| {
                                media_session.matches(path).unwrap_or(0) == path.len() as i32
                            }) {
                                1usize
                            } else {
                                0usize
                            }
                    })
            })
        })
    }

    /// Get the number of clients wanting data for a tag
    pub(crate) async fn get_number_of_clients<T: Into<String>>(&self, tag: T) -> Option<usize> {
        self.imp().get_number_of_clients(tag).await
    }

    /// Returns true once the pause buffer is ready
    pub(crate) async fn buffer_ready<T: Into<String>>(&self, tag: T) -> Option<bool> {
        self.imp().buffer_ready(tag).await
    }

    #[allow(dead_code)]
    /// Clear all sessions on a given path
    pub(crate) async fn clear_session<T: Into<String>>(&self, tag: T) {
        let tag: String = tag.into();
        if let Some(media) = self.imp().medias.read().await.get(&tag) {
            self.clear_session_paths(media.paths.iter());
        }
    }

    fn clear_session_paths<T: AsRef<str>>(&self, paths: impl Iterator<Item = T>) {
        let paths: Vec<_> = paths.collect();
        self.client_filter(Some(&mut |_server, client| {
            let mut close_client = false;
            client.session_filter(Some(&mut |_client, session| {
                let mut clean_up = false;
                session.filter(Some(&mut |_session, session_media| {
                    if paths.iter().any(|p| {
                        let s = p.as_ref();
                        session_media
                            .matches(s)
                            .map(|amt| amt == s.len() as i32)
                            .unwrap_or(false)
                    }) {
                        info!("Removing Session Media");
                        clean_up = true;
                        RTSPFilterResult::Remove
                    } else {
                        RTSPFilterResult::Keep
                    }
                }));

                if clean_up {
                    close_client = true;
                    RTSPFilterResult::Remove
                } else {
                    RTSPFilterResult::Keep
                }
            }));
            if close_client {
                client.close();
                RTSPFilterResult::Remove
            } else {
                RTSPFilterResult::Keep
            }
        }));
    }

    /// Clear all sessions on the rtsp server that are using the not ready stream
    pub(crate) async fn clear_session_notready<T: Into<String>>(&self, tag: T) {
        let tag: String = tag.into();
        if let Some(media) = self.imp().medias.read().await.get(&tag) {
            self.clear_session_paths_notready(media.paths.iter());
        }
    }

    fn clear_session_paths_notready<T: AsRef<str>>(&self, paths: impl Iterator<Item = T>) {
        let paths: Vec<_> = paths.collect();
        self.client_filter(Some(&mut |_server, client| {
            let mut close_client = false;
            client.session_filter(Some(&mut |_client, session| {
                let mut clean_up = false;
                session.filter(Some(&mut |_session, session_media| {
                    if paths.iter().any(|p| {
                        let s = p.as_ref();
                        session_media
                            .matches(s)
                            .map(|amt| amt == s.len() as i32)
                            .unwrap_or(false)
                    }) {
                        // Path is correct now check for no vidsrc
                        let element = session_media
                            .media()
                            .expect("Media should exist")
                            .element()
                            .dynamic_cast::<gstreamer::Bin>()
                            .expect("Media element should be a bin");
                        // debug!("Searching for testvidsrc on {:?}", element.name());
                        // for child in element.children().iter() {
                        //     debug!(" - Child: {:?}", child.name());
                        // }
                        if element.child_by_name("testvidsrc").is_some() {
                            info!("Removing Session Media");
                            clean_up = true;
                            RTSPFilterResult::Remove
                        } else {
                            RTSPFilterResult::Keep
                        }
                    } else {
                        RTSPFilterResult::Keep
                    }
                }));

                if clean_up {
                    close_client = true;
                    RTSPFilterResult::Remove
                } else {
                    RTSPFilterResult::Keep
                }
            }));
            if close_client {
                client.close();
                RTSPFilterResult::Remove
            } else {
                RTSPFilterResult::Keep
            }
        }));
    }

    // Clear buffers on all senders of a tag
    #[allow(dead_code)] // Old function, was used to clear buffer on reconnect
    pub(crate) async fn clear_buffer<T: Into<String>>(&self, tag: T) -> AnyResult<()> {
        if let Some(sender) = self.imp().get_sender(tag).await {
            sender.send(FactoryCommand::ClearBuffer).await?;
            Ok(())
        } else {
            Err(anyhow!("No such tag"))
        }
    }

    // Jump to live on all senders of a tag
    pub(crate) async fn jump_to_live<T: Into<String>>(&self, tag: T) -> AnyResult<()> {
        if let Some(sender) = self.imp().get_sender(tag).await {
            sender.send(FactoryCommand::JumpToLive).await?;
            Ok(())
        } else {
            Err(anyhow!("No such tag"))
        }
    }

    // Pause on all senders of a tag
    pub(crate) async fn pause<T: Into<String>>(&self, tag: T) -> AnyResult<()> {
        if let Some(sender) = self.imp().get_sender(tag).await {
            sender.send(FactoryCommand::Pause).await?;
            Ok(())
        } else {
            Err(anyhow!("No such tag"))
        }
    }

    // Resume on all senders of a tag
    pub(crate) async fn resume<T: Into<String>>(&self, tag: T) -> AnyResult<()> {
        if let Some(sender) = self.imp().get_sender(tag).await {
            sender.send(FactoryCommand::Resume).await?;
            Ok(())
        } else {
            Err(anyhow!("No such tag"))
        }
    }
}

unsafe impl Send for NeoRtspServer {}
unsafe impl Sync for NeoRtspServer {}

struct FactoryData {
    factory: NeoMediaFactory,
    paths: HashSet<String>,
}

#[derive(Default)]
pub(crate) struct NeoRtspServerImpl {
    medias: RwLock<HashMap<String, FactoryData>>,
    threads: RwLock<JoinSet<AnyResult<()>>>,
    main_loop: RwLock<Option<Arc<MainLoop>>>,
}

impl ObjectImpl for NeoRtspServerImpl {}
impl RTSPServerImpl for NeoRtspServerImpl {}

#[object_subclass]
impl ObjectSubclass for NeoRtspServerImpl {
    const NAME: &'static str = "NeoRtspServer";
    type Type = NeoRtspServer;
    type ParentType = RTSPServer;
}

impl NeoRtspServerImpl {
    pub(crate) async fn create_stream<U: Into<String>>(&self, tag: U) -> AnyResult<()> {
        let key = tag.into();
        match self.medias.write().await.entry(key.clone()) {
            Entry::Occupied(_occ) => {}
            Entry::Vacant(vac) => {
                let media = NeoMediaFactory::new();
                let thread_media = media.clone();
                self.threads
                    .write()
                    .await
                    .spawn(async move { thread_media.join().await });
                vac.insert(FactoryData {
                    factory: media,
                    paths: Default::default(),
                });
            }
        };
        Ok(())
    }

    pub(crate) async fn get_sender<T: Into<String>>(
        &self,
        tag: T,
    ) -> Option<Sender<FactoryCommand>> {
        let key = tag.into();
        self.medias
            .read()
            .await
            .get(&key)
            .map(|k| k.factory.get_sender())
    }
    pub(crate) async fn buffer_ready<T: Into<String>>(&self, tag: T) -> Option<bool> {
        let key = tag.into();
        self.medias
            .read()
            .await
            .get(&key)
            .map(|k| k.factory.buffer_ready())
    }

    pub(crate) async fn get_number_of_clients<T: Into<String>>(&self, tag: T) -> Option<usize> {
        let key = tag.into();
        self.medias
            .read()
            .await
            .get(&key)
            .map(|k| k.factory.number_of_clients())
    }

    #[allow(dead_code)]
    pub(crate) async fn remove_stream<T: Into<String>>(&self, tag: T) -> AnyResult<()> {
        if let Some(mut media) = self.medias.write().await.remove(&tag.into()) {
            let mounts = self
                .obj()
                .mount_points()
                .expect("The server should have mountpoints");
            for path in media.paths.iter() {
                mounts.remove_factory(path);
            }
            media.paths.clear();
        }
        Ok(())
    }

    pub(crate) async fn add_permitted_roles<T: Into<String>, U: AsRef<str>>(
        &self,
        tag: T,
        permitted_users: &HashSet<U>,
    ) -> AnyResult<()> {
        let tag: String = tag.into();
        if let Some(media) = self.medias.write().await.get_mut(&tag) {
            media.factory.add_permitted_roles(permitted_users);
            Ok(())
        } else {
            Err(anyhow!("No media with tag {} to add users to", &tag))
        }
    }

    pub(crate) async fn add_path<T: Into<String>>(
        &self,
        tag: T,
        paths: &[String],
    ) -> AnyResult<()> {
        let tag = tag.into();
        if let Some(media) = self.medias.write().await.get_mut(&tag) {
            let mounts = self
                .obj()
                .mount_points()
                .expect("The server should have mountpoints");
            for path in paths {
                media.paths.insert(path.clone());
                // We can clone here because GObjects are referernce counted
                mounts.add_factory(path, media.factory.clone());
                // debug!("Adding path: {}", path);
            }
            Ok(())
        } else {
            Err(anyhow!(
                "No media with tag {} to add the paths {:?} to",
                &tag,
                paths
            ))
        }
    }

    pub(crate) async fn remove_path<T: Into<String>>(
        &self,
        tag: T,
        paths: &[&str],
    ) -> AnyResult<()> {
        let tag = tag.into();
        if let Some(media) = self.medias.write().await.get_mut(&tag) {
            let mounts = self
                .obj()
                .mount_points()
                .expect("The server should have mountpoints");
            for path in paths {
                if media.paths.contains(&path.to_string()) {
                    media.paths.remove(&path.to_string());
                    mounts.remove_factory(path);
                }
            }
            Ok(())
        } else {
            Err(anyhow!(
                "No media with tag {} to remove the paths from",
                &tag
            ))
        }
    }

    pub(crate) fn set_credentials(&self, credentials: &[(&str, &str)]) -> AnyResult<()> {
        let auth = self.obj().auth().unwrap_or_else(RTSPAuth::new);
        auth.set_supported_methods(RTSPAuthMethod::Basic);

        let mut un_authtoken = RTSPToken::new(&[(RTSP_TOKEN_MEDIA_FACTORY_ROLE, &"anonymous")]);
        auth.set_default_token(Some(&mut un_authtoken));

        for credential in credentials {
            let (user, pass) = credential;
            trace!("Setting credentials for user {}", user);
            let token = RTSPToken::new(&[(RTSP_TOKEN_MEDIA_FACTORY_ROLE, user)]);
            let basic = RTSPAuth::make_basic(user, pass);
            auth.add_basic(basic.as_str(), &token);
        }

        self.obj().set_auth(Some(&auth));
        Ok(())
    }

    pub(crate) fn set_tls(
        &self,
        cert_file: &str,
        client_auth: TlsAuthenticationMode,
    ) -> AnyResult<()> {
        debug!("Setting up TLS using {}", cert_file);
        let auth = self.obj().auth().unwrap_or_else(RTSPAuth::new);

        // We seperate reading the file and changing to a PEM so that we get different error messages.
        let cert_contents = fs::read_to_string(cert_file).with_context(|| "TLS file not found")?;
        let cert = TlsCertificate::from_pem(&cert_contents)
            .with_context(|| "Not a valid TLS certificate")?;
        auth.set_tls_certificate(Some(&cert));
        auth.set_tls_authentication_mode(client_auth);

        self.obj().set_auth(Some(&auth));
        Ok(())
    }

    pub(crate) fn set_up_tls(&self, config: &Config) {
        let tls_client_auth = match &config.tls_client_auth as &str {
            "request" => TlsAuthenticationMode::Requested,
            "require" => TlsAuthenticationMode::Required,
            "none" => TlsAuthenticationMode::None,
            _ => unreachable!(),
        };
        if let Some(cert_path) = &config.certificate {
            self.set_tls(cert_path, tls_client_auth)
                .expect("Failed to set up TLS");
        }
    }

    pub(crate) fn set_up_users(&self, users: &[UserConfig]) {
        // Setting up users
        let credentials: Vec<_> = users
            .iter()
            .map(|user| (&*user.name, &*user.pass))
            .collect();
        self.set_credentials(&credentials)
            .expect("Failed to set up users");
    }
}
