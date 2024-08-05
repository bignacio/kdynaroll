use std::sync::LazyLock;
use tokio::runtime::Runtime;

const WORKER_THREADS_COUNT: usize = 2;

pub static CONTROLLER_TASK_RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS_COUNT) // Set the desired number of threads
        .enable_all()
        .build()
        .unwrap()
});
