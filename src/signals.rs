//! Which signals make the console quit (and, in managed mode, stop its node first).

/// Ctrl-C everywhere. On Unix also SIGTERM (Docker, systemd, `kill`) and SIGHUP: closing the terminal the
/// console runs in sends SIGHUP, and without a handler it would kill the console on the spot and leave the
/// node it started orphaned, whatever `stop_node_on_exit` says.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (mut term, mut hup) = match (signal(SignalKind::terminate()), signal(SignalKind::hangup())) {
            (Ok(t), Ok(h)) => (t, h),
            _ => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn ends_after(sig: i32) -> bool {
        let waiter = tokio::spawn(shutdown_signal());
        // Let the task register its handlers before the signal arrives.
        tokio::time::sleep(Duration::from_millis(150)).await;
        unsafe {
            libc::raise(sig);
        }
        tokio::time::timeout(Duration::from_secs(3), waiter).await.is_ok()
    }

    #[tokio::test]
    async fn s5_sighup_is_a_shutdown_signal_like_sigterm() {
        // The handler replaces the default action (which would end the test process).
        assert!(ends_after(libc::SIGHUP).await, "SIGHUP must end shutdown_signal()");
    }

    #[tokio::test]
    async fn sigterm_still_is_one() {
        assert!(ends_after(libc::SIGTERM).await);
    }
}
