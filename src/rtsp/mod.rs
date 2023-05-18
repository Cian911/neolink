///
/// # Neolink RTSP
///
/// This module serves the rtsp streams for the
/// `neolink rtsp` subcommand
///
/// All camera specified in the config.toml will be served
/// over rtsp. By default it bind to all local ip addresses
/// on the port 8554.
///
/// You can view the streams with any rtsp compliement program
/// such as ffmpeg, vlc, blue-iris, home-assistant, zone-minder etc.
///
/// Each camera has it own endpoint based on its name. For example
/// a camera named `"Garage"` in the config can be found at.
///
/// `rtsp://my.ip.address:8554/Garage`
///
/// With the lower resolution stream at
///
/// `rtsp://my.ip.address:8554/Garage/subStream`
///
/// # Usage
///
/// To start the subcommand use the following in a shell.
///
/// ```bash
/// neolink rtsp --config=config.toml
/// ```
///
/// # Example Config
///
/// ```toml
// [[cameras]]
// name = "Cammy"
// username = "****"
// password = "****"
// address = "****:9000"
//   [cameras.pause]
//   on_motion = false
//   on_client = false
//   mode = "none"
//   timeout = 1.0
// ```
//
// - When `on_motion` is true the camera will pause streaming when motion is stopped and resume it when motion is started
// - When `on_client` is true the camera will pause while there is no client connected.
// - `timeout` handels how long to wait after motion stops before pausing the stream
// - `mode` has the following values:
//   - `"black"`: Switches to a black screen. Requires more cpu as the stream is fully reencoded
//   - `"still"`: Switches to a still image. Requires more cpu as the stream is fully reencoded
//   - `"test"`: Switches to the gstreamer test image. Requires more cpu as the stream is fully reencoded
//   - `"none"`: Resends the last iframe the camera. This does not reencode at all.  **Most use cases should use this one as it has the least effort on the cpu and gives what you would expect**
//
use anyhow::{anyhow, Context, Result};
use futures::stream::FuturesUnordered;
use log::*;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;

mod cmdline;
mod gst;
mod spring;
mod states;

use super::config::Config;
pub(crate) use cmdline::Opt;
use gst::NeoRtspServer;
pub(crate) use spring::*;
use states::*;

/// Entry point for the rtsp subcommand
///
/// Opt is the command line options
pub(crate) async fn main(_opt: Opt, mut config: Config) -> Result<()> {
    let rtsp = Arc::new(NeoRtspServer::new()?);

    rtsp.set_up_tls(&config);

    rtsp.set_up_users(&config.users);

    if config.certificate.is_none() && !config.users.is_empty() {
        warn!(
            "Without a server certificate, usernames and passwords will be exchanged in plaintext!"
        )
    }
    let mut cameras = vec![];
    for camera_config in config.cameras.drain(..) {
        cameras
            .push(Camera::<Disconnected>::new(camera_config, &config.users, rtsp.clone()).await?);
    }

    let mut set = tokio::task::JoinSet::new();
    for mut camera in cameras.drain(..) {
        // Spawn each camera controller in it's own thread
        set.spawn(async move {
            let shared = camera.shared.clone();
            let name = camera.get_name();
            let mut backoff = Duration::from_micros(125);
            loop {
                tokio::task::yield_now().await;
                let failure = camera_main(camera).await;
                match failure {
                    Err(CameraFailureKind::Fatal(e)) => {
                        error!("{}: Fatal error: {:?}", name, e);
                        return Err(e);
                    }
                    Err(CameraFailureKind::Retry(e)) => {
                        warn!("{}: Retryable error: {:X?}", name, e);
                        tokio::time::sleep(backoff).await;
                        if backoff < Duration::from_secs(5) {
                            backoff *= 2;
                        }
                        camera = Camera {
                            shared: shared.clone(),
                            state: Disconnected {},
                        };
                    }
                    Ok(()) => {
                        info!("{}: Shutting down", name);
                        break;
                    }
                }
            }
            Ok(())
        });
    }
    info!(
        "Starting RTSP Server at {}:{}",
        &config.bind_addr, config.bind_port,
    );

    let bind_addr = config.bind_addr.clone();
    let bind_port = config.bind_port;
    rtsp.run(&bind_addr, bind_port).await?;
    let thread_rtsp = rtsp.clone();
    set.spawn(async move { thread_rtsp.join().await });

    while let Some(joined) = set.join_next().await {
        match &joined {
            Err(_) | Ok(Err(_)) => {
                // Panicked or error in task
                rtsp.quit().await?;
            }
            Ok(Ok(_)) => {
                // All good
            }
        }
        joined??
    }

    Ok(())
}

enum CameraFailureKind {
    Fatal(anyhow::Error),
    Retry(anyhow::Error),
}

async fn camera_main(camera: Camera<Disconnected>) -> Result<(), CameraFailureKind> {
    // Connect
    let name = camera.get_name();
    let connected = camera
        .connect()
        .await
        .with_context(|| format!("{}: Could not connect to camera", name))
        .map_err(CameraFailureKind::Retry)?;

    let loggedin = connected
        .login()
        .await
        .with_context(|| format!("{}: Could not login to camera", name))
        .map_err(|e| {
            let e_inner = e.downcast_ref::<neolink_core::Error>().unwrap();
            match e_inner {
                neolink_core::Error::CameraLoginFail => CameraFailureKind::Fatal(e),
                _ => CameraFailureKind::Retry(e),
            }
        })?;

    let _ = loggedin.manage().await;

    let tags = loggedin.shared.get_tags();
    let rtsp_thread = loggedin.get_rtsp();

    // Clear all buffers present
    // Uncomment to clear buffers. This is now handlled in the buffer itself,
    // instead of clearing it restamps it whenever there is a jump in the
    // timestamps of >1s
    //
    // tags.iter()
    //     .map(|tag| rtsp_thread.clear_buffer(tag))
    //     .collect::<FuturesUnordered<_>>()
    //     .collect::<Vec<_>>()
    //     .await;

    // Start pulling data from the camera
    let mut streaming = loggedin
        .stream()
        .await
        .with_context(|| format!("{}: Could not start stream", name))
        .map_err(CameraFailureKind::Retry)?;

    // Wait for buffers to be prepared
    tokio::select! {
        v = async {
            let mut waiter = tokio::time::interval(Duration::from_micros(500));
            loop {
                waiter.tick().await;
                if tags
                    .iter()
                    .map(|tag| rtsp_thread.buffer_ready(tag))
                    .collect::<FuturesUnordered<_>>()
                    .all(|f| f.unwrap_or(false))
                    .await
                {
                    break;
                }
            }
            Ok(())
        } => v,
        // Or for stream to error
        v = streaming.join() => {v},
    }
    .with_context(|| format!("{}: Error while waiting for buffers", name))
    .map_err(CameraFailureKind::Retry)?;

    tags.iter()
        .map(|tag| rtsp_thread.jump_to_live(tag))
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;

    // Clear "stream not ready" media to try and force a reconnect
    //   This shoud stop them from watching the "Stream Not Ready" thing
    debug!("Clearing not ready clients");
    tags.iter()
        .map(|tag| rtsp_thread.clear_session_notready(tag))
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;
    log::info!("{}: Buffers prepared", name);

    loop {
        // Wait for error or reason to pause
        tokio::select! {
            v = async {
                // Wait for error
                streaming.join().await
            } => {
                info!("Join Pause");
                v
            },
            v = async {
                // Wait for motion stop
                let mut motion = streaming.get_camera().listen_on_motion().await?;
                motion.await_stop(Duration::from_secs_f64(streaming.get_config().pause.motion_timeout)).await
            }, if streaming.get_config().pause.on_motion => {
                info!("Motion Pause");
                v.map_err(|e| anyhow!("Error while processing motion messages: {:?}", e))
            },
            v = async {
                // Wait for client to disconnect
                let mut inter = tokio::time::interval(tokio::time::Duration::from_secs_f32(0.01));

                loop {
                    inter.tick().await;
                    let total_clients =  tags.iter().map(|tag| rtsp_thread.get_number_of_clients(tag)).collect::<FuturesUnordered<_>>().fold(0usize, |acc, f| acc + f.unwrap_or(0usize)).await;
                    // info!("Num clients: {}", total_clients);
                    if total_clients == 0 {
                        return Ok(())
                    }
                }
            }, if streaming.get_config().pause.on_disconnect => {
                info!("Client Pause");
                v
            },
        }.with_context(|| format!("{}: Error while streaming", name))
        .map_err(CameraFailureKind::Retry)?;

        tags.iter()
            .map(|tag| rtsp_thread.pause(tag))
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await;

        let paused = streaming
            .stop()
            .await
            .with_context(|| format!("{}: Could not stop stream", name))
            .map_err(CameraFailureKind::Retry)?;
        // Wait for reason to restart
        tokio::select! {
            v = async {
                // Wait for motion start and client for start
                let mut motion = paused.get_camera().listen_on_motion().await?;
                let mut inter = tokio::time::interval(tokio::time::Duration::from_secs_f32(0.01));
                loop {
                    motion.await_start(Duration::ZERO).await?;
                    let total_clients =  tags.iter().map(|tag| rtsp_thread.get_number_of_clients(tag)).collect::<FuturesUnordered<_>>().fold(0usize, |acc, f| acc + f.unwrap_or(0usize)).await;
                    if total_clients > 0 {
                        return Result::<()>::Ok(());
                    }
                    inter.tick().await;
                }
            }, if paused.get_config().pause.on_motion && paused.get_config().pause.on_disconnect => {
                info!("Motion and Client Resume");
                v.with_context(|| "Error while processing motion/client messages")
            },
            v = async {
                // Wait for motion start
                let mut motion = paused.get_camera().listen_on_motion().await?;
                motion.await_start(Duration::ZERO).await
            }, if paused.get_config().pause.on_motion && !paused.get_config().pause.on_disconnect => {
                info!("Motion Resume");
                v.with_context(|| "Error while processing motion messages")
            },
            v = async {
                // Wait for client to connect
                let mut inter = tokio::time::interval(tokio::time::Duration::from_secs_f32(0.01));

                loop {
                    inter.tick().await;
                    let total_clients =  tags.iter().map(|tag| rtsp_thread.get_number_of_clients(tag)).collect::<FuturesUnordered<_>>().fold(0usize, |acc, f| acc + f.unwrap_or(0usize)).await;
                    if total_clients > 0 {
                        return Ok(())
                    }
                }
            }, if paused.get_config().pause.on_disconnect && !paused.get_config().pause.on_motion => {
                info!("Client Resume");
                v
            },
            else => {
                // No pause. This means that the stream stopped for some reason
                // but not because of an error
                info!("Generic Resume");
                Ok(())
            }
        }
        .with_context(|| format!("{}: Error while paused", name))
        .map_err(CameraFailureKind::Retry)?;

        tags.iter()
            .map(|tag| rtsp_thread.jump_to_live(tag))
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await;

        streaming = paused
            .stream()
            .await
            .with_context(|| format!("{}: Could not start stream", name))
            .map_err(CameraFailureKind::Retry)?;

        tags.iter()
            .map(|tag| rtsp_thread.resume(tag))
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await;
    }
}
