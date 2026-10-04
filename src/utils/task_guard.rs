//! Stop a helper task when its owner goes away: aborting a task does not abort the tasks it
//! spawned, so a connection's reader task is held in one of these.

pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
