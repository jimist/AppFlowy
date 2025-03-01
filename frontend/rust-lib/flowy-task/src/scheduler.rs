use crate::queue::TaskQueue;
use crate::store::TaskStore;
use crate::{Task, TaskContent, TaskId, TaskState};
use anyhow::Error;
use lib_infra::async_trait::async_trait;
use lib_infra::future::BoxResultFuture;
use lib_infra::ref_map::{RefCountHashMap, RefCountValue};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, RwLock};
use tokio::time::interval;

pub struct TaskDispatcher {
    queue: TaskQueue,
    store: TaskStore,
    timeout: Duration,
    handlers: RefCountHashMap<RefCountTaskHandler>,

    notifier: watch::Sender<bool>,
    pub(crate) notifier_rx: Option<watch::Receiver<bool>>,
}

impl TaskDispatcher {
    pub fn new(timeout: Duration) -> Self {
        let (notifier, notifier_rx) = watch::channel(false);
        Self {
            queue: TaskQueue::new(),
            store: TaskStore::new(),
            timeout,
            handlers: RefCountHashMap::new(),
            notifier,
            notifier_rx: Some(notifier_rx),
        }
    }

    pub fn register_handler<T>(&mut self, handler: T)
    where
        T: TaskHandler,
    {
        let handler_id = handler.handler_id().to_owned();
        self.handlers.insert(handler_id, RefCountTaskHandler(Arc::new(handler)));
    }

    pub async fn unregister_handler<T: AsRef<str>>(&mut self, handler_id: T) {
        self.handlers.remove(handler_id.as_ref()).await;
    }

    pub fn stop(&mut self) {
        let _ = self.notifier.send(true);
        self.queue.clear();
        self.store.clear();
    }

    pub(crate) async fn process_next_task(&mut self) -> Option<()> {
        let pending_task = self.queue.mut_head(|list| list.pop())?;
        let mut task = self.store.remove_task(&pending_task.id)?;
        let ret = task.ret.take()?;

        // Do not execute the task if the task was cancelled.
        if task.state().is_cancel() {
            let _ = ret.send(task.into());
            self.notify();
            return None;
        }

        let content = task.content.take()?;
        if let Some(handler) = self.handlers.get(&task.handler_id) {
            task.set_state(TaskState::Processing);
            match tokio::time::timeout(self.timeout, handler.run(content)).await {
                Ok(result) => match result {
                    Ok(_) => task.set_state(TaskState::Done),
                    Err(e) => {
                        tracing::error!("Process task failed: {:?}", e);
                        task.set_state(TaskState::Failure);
                    }
                },
                Err(e) => {
                    tracing::error!("Process task timeout: {:?}", e);
                    task.set_state(TaskState::Timeout);
                }
            }
        } else {
            task.set_state(TaskState::Cancel);
        }
        let _ = ret.send(task.into());
        self.notify();
        None
    }

    pub fn add_task(&mut self, task: Task) {
        debug_assert!(!task.state().is_done());
        if task.state().is_done() {
            return;
        }

        self.queue.push(&task);
        self.store.insert_task(task);
        self.notify();
    }

    pub fn read_task(&self, task_id: &TaskId) -> Option<&Task> {
        self.store.read_task(task_id)
    }

    pub fn cancel_task(&mut self, task_id: TaskId) {
        if let Some(task) = self.store.mut_task(&task_id) {
            task.set_state(TaskState::Cancel);
        }
    }

    pub fn next_task_id(&self) -> TaskId {
        self.store.next_task_id()
    }

    pub(crate) fn notify(&self) {
        let _ = self.notifier.send(false);
    }
}
pub struct TaskRunner();
impl TaskRunner {
    pub async fn run(dispatcher: Arc<RwLock<TaskDispatcher>>) {
        dispatcher.read().await.notify();
        let debounce_duration = Duration::from_millis(300);
        let mut notifier = dispatcher.write().await.notifier_rx.take().expect("Only take once");
        loop {
            // stops the runner if the notifier was closed.
            if notifier.changed().await.is_err() {
                break;
            }

            // stops the runner if the value of notifier is `true`
            if *notifier.borrow() {
                break;
            }

            let mut interval = interval(debounce_duration);
            interval.tick().await;
            let _ = dispatcher.write().await.process_next_task().await;
        }
    }
}

pub trait TaskHandler: Send + Sync + 'static {
    fn handler_id(&self) -> &str;

    fn run(&self, content: TaskContent) -> BoxResultFuture<(), Error>;
}

impl<T> TaskHandler for Box<T>
where
    T: TaskHandler,
{
    fn handler_id(&self) -> &str {
        (**self).handler_id()
    }

    fn run(&self, content: TaskContent) -> BoxResultFuture<(), Error> {
        (**self).run(content)
    }
}

impl<T> TaskHandler for Arc<T>
where
    T: TaskHandler,
{
    fn handler_id(&self) -> &str {
        (**self).handler_id()
    }

    fn run(&self, content: TaskContent) -> BoxResultFuture<(), Error> {
        (**self).run(content)
    }
}
#[derive(Clone)]
struct RefCountTaskHandler(Arc<dyn TaskHandler>);

#[async_trait]
impl RefCountValue for RefCountTaskHandler {
    async fn did_remove(&self) {}
}

impl std::ops::Deref for RefCountTaskHandler {
    type Target = Arc<dyn TaskHandler>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
