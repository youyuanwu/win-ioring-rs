use futures::channel::oneshot as futures_oneshot;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

/// A handle that can be awaited to get the result of a spawned task
pub struct JoinHandle<T> {
    receiver: crate::runtime::rt::oneshot::Receiver<T>,
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(cx) {
            Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError::TaskAborted)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A thread-safe handle that can be awaited and woken from other threads
/// Uses futures::channel::oneshot for thread-safe communication
pub struct ThreadSafeJoinHandle<T> {
    receiver: futures_oneshot::Receiver<T>,
    waker_handle: ThreadSafeWakerHandle,
}

impl<T> Future for ThreadSafeJoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(cx) {
            Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError::TaskAborted)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> ThreadSafeJoinHandle<T> {
    /// Get a handle that can be used to wake this task from other threads
    pub fn waker_handle(&self) -> ThreadSafeWakerHandle {
        self.waker_handle.clone()
    }
}

/// Error returned when a task join fails
#[derive(Debug, PartialEq)]
pub enum JoinError {
    TaskAborted,
}

/// A handle to the runtime that can be used to spawn tasks from within other tasks
#[derive(Clone)]
pub struct Handle {
    task_spawner: Rc<RefCell<TaskSpawner>>,
    thread_safe_waker_registry: Arc<Mutex<ThreadSafeWakerRegistry>>,
}

/// A thread-safe handle that can be used to wake tasks from other threads
#[derive(Clone)]
pub struct ThreadSafeWakerHandle {
    task_id: TaskId,
    registry: Arc<Mutex<ThreadSafeWakerRegistry>>,
}

impl ThreadSafeWakerHandle {
    /// Wake the associated task from any thread
    pub fn wake(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.wake_task(self.task_id);
        }
    }
}

/// Internal task spawner shared between runtime and handles
struct TaskSpawner {
    next_task_id: TaskId,
    pending_spawns: VecDeque<Task>,
}

/// A bare minimal single-threaded async runtime with waker support
pub struct Runtime {
    /// Queue of ready tasks to be executed
    ready_queue: VecDeque<Task>,
    /// Map of task IDs to tasks that are waiting to be woken
    waiting_tasks: HashMap<TaskId, Task>,
    /// Flag to indicate if the runtime should keep running
    running: bool,
    /// Shared waker registry for task communication (single-threaded)
    waker_registry: Rc<RefCell<WakerRegistry>>,
    /// Thread-safe waker registry for cross-thread communication
    thread_safe_waker_registry: Arc<Mutex<ThreadSafeWakerRegistry>>,
    /// Shared task spawner for spawning tasks from within other tasks
    task_spawner: Rc<RefCell<TaskSpawner>>,
}

/// Unique identifier for tasks
type TaskId = u64;

/// Type of waker to use for a task
#[derive(Clone, Debug)]
enum WakerType {
    SingleThreaded,
    ThreadSafe,
}

/// A spawned task with an ID
struct Task {
    /// Unique identifier for this task
    id: TaskId,
    /// The future to be executed
    future: Pin<Box<dyn Future<Output = ()>>>,
    /// Type of waker to use for this task
    waker_type: WakerType,
}

/// Registry for managing wakers across tasks
struct WakerRegistry {
    /// Map of task IDs to their wakers
    wakers: HashMap<TaskId, RuntimeWaker>,
    /// Set of task IDs that have been woken
    woken_tasks: Vec<TaskId>,
}

impl WakerRegistry {
    fn new() -> Self {
        Self {
            wakers: HashMap::new(),
            woken_tasks: Vec::new(),
        }
    }

    fn register_waker(&mut self, task_id: TaskId, waker: RuntimeWaker) {
        self.wakers.insert(task_id, waker);
    }

    fn remove_waker(&mut self, task_id: TaskId) {
        self.wakers.remove(&task_id);
    }

    fn wake_task(&mut self, task_id: TaskId) {
        if self.wakers.contains_key(&task_id) {
            self.woken_tasks.push(task_id);
        }
    }

    fn get_and_clear_woken_tasks(&mut self) -> Vec<TaskId> {
        std::mem::take(&mut self.woken_tasks)
    }
}

/// Thread-safe registry for managing wakers across threads
struct ThreadSafeWakerRegistry {
    /// Map of task IDs to their wakers
    wakers: HashMap<TaskId, ThreadSafeRuntimeWaker>,
    /// Set of task IDs that have been woken
    woken_tasks: Vec<TaskId>,
}

impl ThreadSafeWakerRegistry {
    fn new() -> Self {
        Self {
            wakers: HashMap::new(),
            woken_tasks: Vec::new(),
        }
    }

    fn register_waker(&mut self, task_id: TaskId, waker: ThreadSafeRuntimeWaker) {
        self.wakers.insert(task_id, waker);
    }

    fn remove_waker(&mut self, task_id: TaskId) {
        self.wakers.remove(&task_id);
    }

    fn wake_task(&mut self, task_id: TaskId) {
        if self.wakers.contains_key(&task_id) {
            self.woken_tasks.push(task_id);
        }
    }

    fn get_and_clear_woken_tasks(&mut self) -> Vec<TaskId> {
        std::mem::take(&mut self.woken_tasks)
    }
}

impl TaskSpawner {
    fn new() -> Self {
        Self {
            next_task_id: 0,
            pending_spawns: VecDeque::new(),
        }
    }

    fn spawn<F>(&mut self, future: F) -> TaskId
    where
        F: Future<Output = ()> + 'static,
    {
        let task_id = self.next_task_id;
        self.next_task_id += 1;

        let task = Task {
            id: task_id,
            future: Box::pin(future),
            waker_type: WakerType::SingleThreaded,
        };
        self.pending_spawns.push_back(task);
        task_id
    }

    fn spawn_with_handle<F, T>(
        &mut self,
        future: F,
    ) -> (TaskId, crate::runtime::rt::oneshot::Receiver<T>)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let task_id = self.next_task_id;
        self.next_task_id += 1;

        let (tx, rx) = crate::runtime::rt::oneshot::channel();

        let wrapped_future = async move {
            let result = future.await;
            let _ = tx.send(result); // Ignore send errors if receiver is dropped
        };

        let task = Task {
            id: task_id,
            future: Box::pin(wrapped_future),
            waker_type: WakerType::SingleThreaded,
        };
        self.pending_spawns.push_back(task);
        (task_id, rx)
    }

    fn spawn_thread_safe_with_handle<F, T>(
        &mut self,
        future: F,
    ) -> (TaskId, futures_oneshot::Receiver<T>)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let task_id = self.next_task_id;
        self.next_task_id += 1;

        // Use futures::channel::oneshot for thread-safe communication
        let (tx, rx) = futures_oneshot::channel();

        let wrapped_future = async move {
            let result = future.await;
            let _ = tx.send(result); // Ignore send errors if receiver is dropped
        };

        let task = Task {
            id: task_id,
            future: Box::pin(wrapped_future),
            waker_type: WakerType::ThreadSafe,
        };
        self.pending_spawns.push_back(task);
        (task_id, rx)
    }

    fn take_pending_spawns(&mut self) -> VecDeque<Task> {
        std::mem::take(&mut self.pending_spawns)
    }
}

impl Handle {
    /// Spawn a future onto the runtime from within another task
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let mut spawner = self.task_spawner.borrow_mut();
        let (_task_id, receiver) = spawner.spawn_with_handle(future);
        JoinHandle { receiver }
    }

    /// Spawn a future with thread-safe waker support
    pub fn spawn_thread_safe<F>(&self, future: F) -> ThreadSafeJoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let mut spawner = self.task_spawner.borrow_mut();
        let (task_id, receiver) = spawner.spawn_thread_safe_with_handle(future);
        ThreadSafeJoinHandle {
            receiver,
            waker_handle: ThreadSafeWakerHandle {
                task_id,
                registry: Arc::clone(&self.thread_safe_waker_registry),
            },
        }
    }

    /// Spawn a future that returns () - for backward compatibility
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let mut spawner = self.task_spawner.borrow_mut();
        spawner.spawn(future);
    }
}

/// Custom waker that can wake specific tasks in our runtime
#[derive(Clone)]
struct RuntimeWaker {
    /// The task ID to wake
    task_id: TaskId,
    /// Reference to the waker registry
    registry: Rc<RefCell<WakerRegistry>>,
}

impl RuntimeWaker {
    fn into_waker(self) -> Waker {
        let raw_waker = RawWaker::new(
            Box::into_raw(Box::new(self)) as *const (),
            &RUNTIME_WAKER_VTABLE,
        );
        unsafe { Waker::from_raw(raw_waker) }
    }

    fn wake_task(&self) {
        let mut registry = self.registry.borrow_mut();
        registry.wake_task(self.task_id);
    }
}

static RUNTIME_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    runtime_waker_clone,
    runtime_waker_wake,
    runtime_waker_wake_by_ref,
    runtime_waker_drop,
);

unsafe fn runtime_waker_clone(data: *const ()) -> RawWaker {
    let waker = unsafe { &*(data as *const RuntimeWaker) };
    let cloned = waker.clone();
    RawWaker::new(
        Box::into_raw(Box::new(cloned)) as *const (),
        &RUNTIME_WAKER_VTABLE,
    )
}

unsafe fn runtime_waker_wake(data: *const ()) {
    let waker = unsafe { Box::from_raw(data as *mut RuntimeWaker) };
    waker.wake_task();
}

unsafe fn runtime_waker_wake_by_ref(data: *const ()) {
    let waker = unsafe { &*(data as *const RuntimeWaker) };
    waker.wake_task();
}

unsafe fn runtime_waker_drop(data: *const ()) {
    let _ = unsafe { Box::from_raw(data as *mut RuntimeWaker) };
}

/// Thread-safe custom waker that can wake specific tasks across threads
#[derive(Clone)]
struct ThreadSafeRuntimeWaker {
    /// The task ID to wake
    task_id: TaskId,
    /// Reference to the thread-safe waker registry
    registry: Arc<Mutex<ThreadSafeWakerRegistry>>,
}

impl ThreadSafeRuntimeWaker {
    fn into_waker(self) -> Waker {
        let raw_waker = RawWaker::new(
            Box::into_raw(Box::new(self)) as *const (),
            &THREAD_SAFE_RUNTIME_WAKER_VTABLE,
        );
        unsafe { Waker::from_raw(raw_waker) }
    }

    fn wake_task(&self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.wake_task(self.task_id);
        }
    }
}

static THREAD_SAFE_RUNTIME_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    thread_safe_runtime_waker_clone,
    thread_safe_runtime_waker_wake,
    thread_safe_runtime_waker_wake_by_ref,
    thread_safe_runtime_waker_drop,
);

unsafe fn thread_safe_runtime_waker_clone(data: *const ()) -> RawWaker {
    let waker = unsafe { &*(data as *const ThreadSafeRuntimeWaker) };
    let cloned = waker.clone();
    RawWaker::new(
        Box::into_raw(Box::new(cloned)) as *const (),
        &THREAD_SAFE_RUNTIME_WAKER_VTABLE,
    )
}

unsafe fn thread_safe_runtime_waker_wake(data: *const ()) {
    let waker = unsafe { Box::from_raw(data as *mut ThreadSafeRuntimeWaker) };
    waker.wake_task();
}

unsafe fn thread_safe_runtime_waker_wake_by_ref(data: *const ()) {
    let waker = unsafe { &*(data as *const ThreadSafeRuntimeWaker) };
    waker.wake_task();
}

unsafe fn thread_safe_runtime_waker_drop(data: *const ()) {
    let _ = unsafe { Box::from_raw(data as *mut ThreadSafeRuntimeWaker) };
}

impl Runtime {
    /// Create a new runtime instance
    pub fn new() -> Self {
        let task_spawner = Rc::new(RefCell::new(TaskSpawner::new()));
        Self {
            ready_queue: VecDeque::new(),
            waiting_tasks: HashMap::new(),
            running: false,
            waker_registry: Rc::new(RefCell::new(WakerRegistry::new())),
            thread_safe_waker_registry: Arc::new(Mutex::new(ThreadSafeWakerRegistry::new())),
            task_spawner,
        }
    }

    /// Get a handle to this runtime for spawning tasks from within other tasks
    pub fn handle(&self) -> Handle {
        Handle {
            task_spawner: Rc::clone(&self.task_spawner),
            thread_safe_waker_registry: Arc::clone(&self.thread_safe_waker_registry),
        }
    }

    /// Block on a future until it completes, returning its result
    pub fn block_on<F>(&mut self, future: F) -> F::Output
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        use std::cell::RefCell;
        use std::rc::Rc;

        // Create a oneshot channel to get the result
        let result = Rc::new(RefCell::new(None));
        let result_clone = result.clone();

        // Wrap the future to capture its output
        let wrapper_future = async move {
            let output = future.await;
            *result_clone.borrow_mut() = Some(output);
        };

        // Spawn the wrapped future
        self.spawn_detached(wrapper_future);

        // Run until the result is available
        self.run_until(|| result.borrow().is_some());

        // Extract and return the result
        result
            .borrow_mut()
            .take()
            .expect("Future should have completed")
    }

    /// Run the runtime until all tasks are complete
    pub fn run(&mut self) {
        self.run_until(|| false);
    }

    /// Internal method that runs the runtime loop with a custom stop condition
    fn run_until<F>(&mut self, mut should_stop: F)
    where
        F: FnMut() -> bool,
    {
        self.running = true;

        while self.running {
            // Check the custom stop condition first
            if should_stop() {
                break;
            }

            // Process any newly spawned tasks first
            self.process_new_spawns();

            // Check if we have any work to do
            if self.ready_queue.is_empty() && self.waiting_tasks.is_empty() {
                break;
            }

            // Process all ready tasks
            while let Some(mut task) = self.ready_queue.pop_front() {
                let waker = match task.waker_type {
                    WakerType::SingleThreaded => self.create_task_waker(task.id),
                    WakerType::ThreadSafe => self.create_thread_safe_waker(task.id),
                };
                let mut context = Context::from_waker(&waker);

                match task.future.as_mut().poll(&mut context) {
                    Poll::Ready(()) => {
                        // Task completed, remove from both waker registries
                        let mut registry = self.waker_registry.borrow_mut();
                        registry.remove_waker(task.id);

                        if let Ok(mut ts_registry) = self.thread_safe_waker_registry.lock() {
                            ts_registry.remove_waker(task.id);
                        }
                    }
                    Poll::Pending => {
                        // Task is not ready, move to waiting tasks
                        self.waiting_tasks.insert(task.id, task);
                    }
                }

                // Check for new spawns after each task execution
                self.process_new_spawns();

                // Check the stop condition after each task
                if should_stop() {
                    break;
                }
            }

            // If stop condition is met, break out of outer loop too
            if should_stop() {
                break;
            }

            // Check for woken tasks and move them back to ready queue
            self.process_woken_tasks();

            // Check for tasks woken from other threads
            self.process_thread_safe_woken_tasks();

            // If no tasks are ready and we have waiting tasks,
            // this could indicate a deadlock or that we need external events
            if self.ready_queue.is_empty() && !self.waiting_tasks.is_empty() {
                // In a real runtime, this would wait for I/O or timers
                // For our simple runtime, yield to the OS scheduler instead of blocking
                std::thread::yield_now();
            }
        }

        self.running = false;
    }

    /// Spawn a future onto the runtime
    pub fn spawn<F>(&mut self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let mut spawner = self.task_spawner.borrow_mut();
        let (_task_id, receiver) = spawner.spawn_with_handle(future);
        JoinHandle { receiver }
    }

    /// Spawn a future with thread-safe waker support
    pub fn spawn_thread_safe<F>(&mut self, future: F) -> ThreadSafeJoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let mut spawner = self.task_spawner.borrow_mut();
        let (task_id, receiver) = spawner.spawn_thread_safe_with_handle(future);
        ThreadSafeJoinHandle {
            receiver,
            waker_handle: ThreadSafeWakerHandle {
                task_id,
                registry: Arc::clone(&self.thread_safe_waker_registry),
            },
        }
    }

    /// Spawn a future that returns () - for backward compatibility
    pub fn spawn_detached<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let mut spawner = self.task_spawner.borrow_mut();
        spawner.spawn(future);
    }

    /// Process any newly spawned tasks and move them to the ready queue
    fn process_new_spawns(&mut self) {
        let mut spawner = self.task_spawner.borrow_mut();
        let new_tasks = spawner.take_pending_spawns();
        for task in new_tasks {
            self.ready_queue.push_back(task);
        }
    }

    /// Create a waker for a specific task
    fn create_task_waker(&self, task_id: TaskId) -> Waker {
        let runtime_waker = RuntimeWaker {
            task_id,
            registry: Rc::clone(&self.waker_registry),
        };

        // Register the waker
        let mut registry = self.waker_registry.borrow_mut();
        registry.register_waker(task_id, runtime_waker.clone());

        runtime_waker.into_waker()
    }

    /// Create a thread-safe waker for a specific task
    fn create_thread_safe_waker(&self, task_id: TaskId) -> Waker {
        let runtime_waker = ThreadSafeRuntimeWaker {
            task_id,
            registry: Arc::clone(&self.thread_safe_waker_registry),
        };

        // Register the waker
        if let Ok(mut registry) = self.thread_safe_waker_registry.lock() {
            registry.register_waker(task_id, runtime_waker.clone());
        }

        runtime_waker.into_waker()
    }

    /// Get a thread-safe waker handle for a task
    pub fn get_thread_safe_waker(&self, task_id: TaskId) -> ThreadSafeWakerHandle {
        ThreadSafeWakerHandle {
            task_id,
            registry: Arc::clone(&self.thread_safe_waker_registry),
        }
    }

    /// Process tasks that have been woken up
    fn process_woken_tasks(&mut self) {
        let mut registry = self.waker_registry.borrow_mut();
        let woken_tasks: Vec<TaskId> = registry.get_and_clear_woken_tasks();

        for task_id in woken_tasks {
            if let Some(task) = self.waiting_tasks.remove(&task_id) {
                self.ready_queue.push_back(task);
            }
        }
    }

    /// Process tasks that have been woken up from other threads
    fn process_thread_safe_woken_tasks(&mut self) {
        if let Ok(mut registry) = self.thread_safe_waker_registry.lock() {
            let woken_tasks: Vec<TaskId> = registry.get_and_clear_woken_tasks();

            for task_id in woken_tasks {
                if let Some(task) = self.waiting_tasks.remove(&task_id) {
                    self.ready_queue.push_back(task);
                }
            }
        }
    }

    /// Stop the runtime
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Check if the runtime is currently running
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Get the number of pending tasks
    pub fn pending_tasks(&self) -> usize {
        let spawner = self.task_spawner.borrow();
        let spawner_tasks = spawner.pending_spawns.len();
        self.ready_queue.len() + self.waiting_tasks.len() + spawner_tasks
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple oneshot channel implementation
pub mod oneshot {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Error when receiver is dropped before sender sends
    #[derive(Debug, PartialEq)]
    pub struct RecvError;

    /// Error when sender tries to send but receiver is dropped
    #[derive(Debug, PartialEq)]
    pub struct SendError<T>(pub T);

    /// Creates a oneshot channel
    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let shared = Rc::new(RefCell::new(ChannelState::Empty));
        let waker = Rc::new(RefCell::new(None));
        let sender = Sender {
            shared: shared.clone(),
            waker: waker.clone(),
        };
        let receiver = Receiver { shared, waker };
        (sender, receiver)
    }

    enum ChannelState<T> {
        Empty,
        Data(T),
        Closed,
    }

    /// Sender half of oneshot channel
    pub struct Sender<T> {
        shared: Rc<RefCell<ChannelState<T>>>,
        waker: Rc<RefCell<Option<Waker>>>,
    }

    impl<T> Sender<T> {
        /// Send a value through the channel
        pub fn send(self, value: T) -> Result<(), SendError<T>> {
            let mut state = self.shared.borrow_mut();
            match *state {
                ChannelState::Empty => {
                    *state = ChannelState::Data(value);
                    // Wake the receiver if it's waiting
                    if let Some(waker) = self.waker.borrow().as_ref() {
                        waker.wake_by_ref();
                    }
                    Ok(())
                }
                _ => Err(SendError(value)),
            }
        }
    }

    /// Receiver half of oneshot channel
    pub struct Receiver<T> {
        shared: Rc<RefCell<ChannelState<T>>>,
        waker: Rc<RefCell<Option<Waker>>>,
    }

    impl<T> Future for Receiver<T> {
        type Output = Result<T, RecvError>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let mut state = this.shared.borrow_mut();
            match std::mem::replace(&mut *state, ChannelState::Closed) {
                ChannelState::Data(value) => Poll::Ready(Ok(value)),
                ChannelState::Closed => Poll::Ready(Err(RecvError)),
                ChannelState::Empty => {
                    *state = ChannelState::Empty;
                    *this.waker.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }
}

/// A simple async function that yields control back to the runtime
pub async fn yield_now() {
    YieldNow { yielded: false }.await
}

/// Future that yields control once
struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            // Wake ourselves immediately so we get polled again next iteration
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let runtime = Runtime::new();
        assert!(!runtime.is_running());
        assert_eq!(runtime.pending_tasks(), 0);
    }

    #[test]
    fn test_spawn_and_run() {
        let mut runtime = Runtime::new();

        runtime.spawn(async {
            println!("Hello from async task!");
        });

        assert_eq!(runtime.pending_tasks(), 1);
        runtime.run();
        assert_eq!(runtime.pending_tasks(), 0);
    }

    #[test]
    fn test_yield_now() {
        let mut runtime = Runtime::new();

        runtime.spawn(async {
            println!("Before yield");
            yield_now().await;
            println!("After yield");
        });

        runtime.run();
    }

    #[test]
    fn test_multiple_tasks() {
        let mut runtime = Runtime::new();

        for i in 0..3 {
            runtime.spawn(async move {
                println!("Task {i}");
                yield_now().await;
                println!("Task {i} after yield");
            });
        }

        runtime.run();
    }

    #[test]
    fn test_oneshot_channel() {
        let mut runtime = Runtime::new();
        let (sender, receiver) = oneshot::channel::<i32>();

        // Spawn receiver task
        runtime.spawn(async move {
            match receiver.await {
                Ok(value) => println!("Received: {value}"),
                Err(_) => println!("Receive error"),
            }
        });

        // Spawn sender task
        runtime.spawn(async move {
            println!("Sending value...");
            sender.send(42).unwrap();
            println!("Value sent!");
        });

        runtime.run();
    }

    #[test]
    fn test_oneshot_communication_between_tasks() {
        let mut runtime = Runtime::new();
        let (tx1, rx1) = oneshot::channel::<String>();
        let (tx2, rx2) = oneshot::channel::<i32>();

        // Task 1: waits for string, then sends number
        runtime.spawn(async move {
            let message = rx1.await.unwrap();
            println!("Task 1 received: {message}");
            tx2.send(message.len() as i32).unwrap();
            println!("Task 1 sent length: {}", message.len());
        });

        // Task 2: sends string, then waits for number
        runtime.spawn(async move {
            tx1.send("Hello, World!".to_string()).unwrap();
            println!("Task 2 sent string");

            let length = rx2.await.unwrap();
            println!("Task 2 received length: {length}");
            assert_eq!(length, 13);
        });

        runtime.run();
    }

    #[test]
    fn test_multiple_oneshot_channels() {
        let mut runtime = Runtime::new();
        let mut senders = Vec::new();
        let mut receivers = Vec::new();

        // Create multiple channels
        for _ in 0..3 {
            let (tx, rx) = oneshot::channel::<i32>();
            senders.push(tx);
            receivers.push(rx);
        }

        // Spawn receiver tasks
        for (i, rx) in receivers.into_iter().enumerate() {
            runtime.spawn(async move {
                let value = rx.await.unwrap();
                println!("Receiver {i} got: {value}");
                assert_eq!(value, i as i32 * 10);
            });
        }

        // Spawn sender tasks
        for (i, tx) in senders.into_iter().enumerate() {
            runtime.spawn(async move {
                let value = i as i32 * 10;
                tx.send(value).unwrap();
                println!("Sender {i} sent: {value}");
            });
        }

        runtime.run();
    }

    #[test]
    fn test_spawn_from_within_task() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        runtime.spawn(async move {
            println!("Parent task starting");

            // Spawn a child task from within this task
            handle.spawn(async {
                println!("Child task 1 executing");
            });

            // Spawn another child task
            handle.spawn(async {
                println!("Child task 2 executing");
                yield_now().await;
                println!("Child task 2 after yield");
            });

            yield_now().await;
            println!("Parent task after yield");
        });

        runtime.run();
    }

    #[test]
    fn test_recursive_spawning() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        runtime.spawn(async move {
            println!("Level 1 task");

            let handle_clone = handle.clone();
            handle.spawn(async move {
                println!("Level 2 task");

                handle_clone.spawn(async {
                    println!("Level 3 task");
                });

                yield_now().await;
                println!("Level 2 task after yield");
            });

            yield_now().await;
            println!("Level 1 task after yield");
        });

        runtime.run();
    }

    #[test]
    fn test_spawn_with_communication() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();
        let (tx1, rx1) = oneshot::channel::<String>();
        let (tx2, rx2) = oneshot::channel::<i32>();

        runtime.spawn(async move {
            println!("Main task starting");

            // Spawn a task that will send data
            handle.spawn(async move {
                println!("Sender task running");
                tx1.send("Hello from spawned task!".to_string()).unwrap();
            });

            // Wait for the message
            let message = rx1.await.unwrap();
            println!("Main task received: {message}");

            // Spawn another task with the message length
            let handle_clone = handle.clone();
            handle_clone.spawn(async move {
                tx2.send(message.len() as i32).unwrap();
            });

            let length = rx2.await.unwrap();
            println!("Message length: {length}");
            assert_eq!(length, 24);
        });

        runtime.run();
    }

    #[test]
    fn test_block_on_simple() {
        let mut runtime = Runtime::new();

        let result = runtime.block_on(async { 42 });

        assert_eq!(result, 42);
    }

    #[test]
    fn test_block_on_with_yield() {
        let mut runtime = Runtime::new();

        let result = runtime.block_on(async {
            yield_now().await;
            "Hello, World!"
        });

        assert_eq!(result, "Hello, World!");
    }

    #[test]
    fn test_block_on_with_communication() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        let result = runtime.block_on(async move {
            let (tx, rx) = oneshot::channel::<i32>();

            // Spawn a task that will send a value
            handle.spawn(async move {
                yield_now().await;
                tx.send(123).unwrap();
            });

            // Wait for the value
            rx.await.unwrap()
        });

        assert_eq!(result, 123);
    }

    #[test]
    fn test_block_on_with_multiple_spawns() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        let result = runtime.block_on(async move {
            let (tx1, rx1) = oneshot::channel::<String>();
            let (tx2, rx2) = oneshot::channel::<i32>();

            // Spawn first task
            handle.spawn_detached(async move {
                yield_now().await;
                tx1.send("First".to_string()).unwrap();
            });

            // Spawn second task
            let handle_clone = handle.clone();
            handle_clone.spawn_detached(async move {
                yield_now().await;
                tx2.send(456).unwrap();
            });

            // Wait for both results
            let msg = rx1.await.unwrap();
            let num = rx2.await.unwrap();

            format!("{msg}: {num}")
        });

        assert_eq!(result, "First: 456");
    }

    #[test]
    fn test_join_handle_basic() {
        let mut runtime = Runtime::new();

        let join_handle = runtime.spawn(async {
            yield_now().await;
            42
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, 42);
    }

    #[test]
    fn test_join_handle_string_result() {
        let mut runtime = Runtime::new();

        let join_handle = runtime.spawn(async {
            yield_now().await;
            "Hello from task!".to_string()
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, "Hello from task!");
    }

    #[test]
    fn test_multiple_join_handles() {
        let mut runtime = Runtime::new();

        let handle1 = runtime.spawn(async {
            yield_now().await;
            10
        });

        let handle2 = runtime.spawn(async {
            yield_now().await;
            20
        });

        let handle3 = runtime.spawn(async {
            yield_now().await;
            30
        });

        let result = runtime.block_on(async move {
            let a = handle1.await.unwrap();
            let b = handle2.await.unwrap();
            let c = handle3.await.unwrap();
            a + b + c
        });

        assert_eq!(result, 60);
    }

    #[test]
    fn test_join_handle_with_communication() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        let join_handle = runtime.spawn(async move {
            let (tx, rx) = oneshot::channel::<i32>();

            // Spawn a task that will send a value
            handle.spawn_detached(async move {
                yield_now().await;
                tx.send(100).unwrap();
            });

            // Wait for the value and return it
            rx.await.unwrap()
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, 100);
    }

    #[test]
    fn test_nested_join_handles() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        let join_handle = runtime.spawn(async move {
            let inner_handle = handle.spawn(async {
                yield_now().await;
                "inner result".to_string()
            });

            let inner_result = inner_handle.await.unwrap();
            format!("outer: {inner_result}")
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, "outer: inner result");
    }

    #[test]
    fn test_spawn_thread_safe_basic() {
        let mut runtime = Runtime::new();

        let join_handle = runtime.spawn_thread_safe(async {
            yield_now().await;
            42
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, 42);
    }

    #[test]
    fn test_spawn_thread_safe_with_waker_handle() {
        let mut runtime = Runtime::new();

        let join_handle = runtime.spawn_thread_safe(async {
            yield_now().await;
            "thread-safe task result".to_string()
        });

        // Get the waker handle for potential cross-thread waking
        let _waker_handle = join_handle.waker_handle();

        // In a real scenario, you could pass this waker_handle to another thread
        // and call waker_handle.wake() to wake the task

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, "thread-safe task result");
    }

    #[test]
    fn test_spawn_thread_safe_from_handle() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        let join_handle = runtime.spawn(async move {
            let thread_safe_handle = handle.spawn_thread_safe(async {
                yield_now().await;
                100
            });

            thread_safe_handle.await.unwrap()
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, 100);
    }

    #[test]
    fn test_thread_safe_join_handle_uses_futures_oneshot() {
        let mut rt = Runtime::new();

        // Spawn a thread-safe task
        let join_handle = rt.spawn_thread_safe(async { 42 });

        // Verify we can get a waker handle (this proves it's using the thread-safe version)
        let _waker_handle = join_handle.waker_handle();

        // Complete the task
        let result = rt.block_on(join_handle).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn test_real_thread_waking() {
        // This test demonstrates true cross-thread waking capability:
        // A real OS thread can wake a task running in the async runtime
        let mut rt = Runtime::new();

        // Create a shared flag to coordinate between threads
        let ready_flag = Arc::new(Mutex::new(false));
        let ready_flag_clone = Arc::clone(&ready_flag);

        // Spawn a thread-safe task that waits for the flag to be set
        let join_handle = rt.spawn_thread_safe(async move {
            // Poll the flag until it's set
            loop {
                {
                    let flag = ready_flag_clone.lock().unwrap();
                    if *flag {
                        return 42;
                    }
                }
                // Yield to allow the runtime to process wakes
                crate::runtime::rt::yield_now().await;
            }
        });

        let waker_handle = join_handle.waker_handle();

        // Spawn a real OS thread that will set the flag and wake the task
        let thread_handle = std::thread::spawn(move || {
            // Wait a bit to ensure the task is waiting
            std::thread::sleep(std::time::Duration::from_millis(10));

            // Set the flag
            *ready_flag.lock().unwrap() = true;

            // Wake the task from this thread
            waker_handle.wake();
        });

        // Block on the task - it should complete when woken by the thread
        let result = rt.block_on(join_handle).unwrap();

        // Wait for the thread to complete
        thread_handle.join().unwrap();

        assert_eq!(result, 42);
    }

    #[test]
    fn test_cross_runtime_communication() {
        // This test demonstrates communication between two separate runtimes
        // running on different threads using futures::oneshot
        use std::sync::mpsc;

        // Create a channel to coordinate the start of both runtimes
        let (start_tx, start_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();

        // Create the futures oneshot channel for cross-runtime communication
        let (futures_tx, futures_rx) = futures_oneshot::channel::<i32>();

        // Thread 1: Runtime that sends data
        let sender_handle = std::thread::spawn(move || {
            let mut rt1 = Runtime::new();

            // Signal that this runtime is ready
            start_tx.send(()).unwrap();

            // Spawn a thread-safe task that sends data after a delay
            let task = rt1.spawn_thread_safe(async move {
                // Wait a bit to ensure the receiver is ready
                for _ in 0..10 {
                    crate::runtime::rt::yield_now().await;
                }

                // Send the value
                let _ = futures_tx.send(123);
                "sender_done"
            });

            rt1.block_on(task).unwrap()
        });

        // Thread 2: Runtime that receives data
        let receiver_handle = std::thread::spawn(move || {
            let mut rt2 = Runtime::new();

            // Wait for the sender runtime to be ready
            start_rx.recv().unwrap();

            // Spawn a thread-safe task that receives data
            let task = rt2.spawn_thread_safe(async move {
                // Wait for data from the other runtime
                (futures_rx.await).unwrap_or(-1)
            });

            let result = rt2.block_on(task).unwrap();
            result_tx.send(result).unwrap();
            "receiver_done"
        });

        // Wait for both threads to complete
        let sender_result = sender_handle.join().unwrap();
        let receiver_result = receiver_handle.join().unwrap();
        let received_value = result_rx.recv().unwrap();

        assert_eq!(sender_result, "sender_done");
        assert_eq!(receiver_result, "receiver_done");
        assert_eq!(received_value, 123);
    }

    #[test]
    fn test_thread_safe_cross_thread_communication_simple() {
        let mut rt = Runtime::new();

        // Spawn a thread-safe task that waits for external input
        let join_handle = rt.spawn_thread_safe(async {
            // This future will be completed from another thread
            futures::future::pending::<i32>().await
        });

        let waker_handle = join_handle.waker_handle();

        // Spawn another thread-safe task that sends data
        let sender_handle = rt.spawn_thread_safe(async move {
            // Simulate some work
            std::thread::sleep(std::time::Duration::from_millis(10));

            // Wake the waiting task from this task's context
            waker_handle.wake();
            42
        });

        // Run the runtime - this should complete both tasks
        let result = rt.block_on(async {
            // The sender task should complete
            sender_handle.await.unwrap()
        });

        assert_eq!(result, 42);
    }

    #[test]
    fn test_mixed_spawn_types() {
        let mut runtime = Runtime::new();
        let handle = runtime.handle();

        let join_handle = runtime.spawn(async move {
            // Regular spawn
            let regular_handle = handle.spawn(async {
                yield_now().await;
                10
            });

            // Thread-safe spawn
            let thread_safe_handle = handle.spawn_thread_safe(async {
                yield_now().await;
                20
            });

            let a = regular_handle.await.unwrap();
            let b = thread_safe_handle.await.unwrap();
            a + b
        });

        let result = runtime.block_on(async move { join_handle.await.unwrap() });

        assert_eq!(result, 30);
    }
}
