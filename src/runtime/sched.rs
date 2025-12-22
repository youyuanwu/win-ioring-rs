/// Example for function yield_now
#[cfg(test)]
mod yield_tests {
    use std::sync::{LazyLock, Mutex, atomic::AtomicI32};

    static DATA1: LazyLock<Mutex<Vec<i32>>> = LazyLock::new(|| Mutex::new(vec![]));
    static DATA2: LazyLock<Mutex<Vec<i32>>> = LazyLock::new(|| Mutex::new(vec![]));
    static COUNTER: AtomicI32 = AtomicI32::new(0);
    fn foo() {
        let val = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            DATA1.lock().unwrap().push(val);
        }
        // yield to other tasks
        EXECUTOR.yield_now();
        {
            DATA2.lock().unwrap().push(val);
        }
    }

    struct Executor {
        tasks: Mutex<Vec<Box<dyn FnOnce() + 'static + Send>>>,
    }

    impl Executor {
        fn new() -> Self {
            Self {
                tasks: Mutex::new(vec![]),
            }
        }

        fn spawn<F>(&self, f: F)
        where
            F: FnOnce() + 'static + Send,
        {
            self.tasks.lock().unwrap().push(Box::new(f));
        }

        fn run(&self) {
            loop {
                let task = self.tasks.lock().unwrap().pop();
                if let Some(task) = task {
                    task();
                } else {
                    break;
                }
            }
        }

        // Proper implementation requires tracking task progress and usage,
        // and re-queuing tasks appropriately. This is for demonstration only.
        // This can overflow the stack if tasks yield too many times without completing.
        fn yield_now(&self) {
            // run the next task.
            let task = self.tasks.lock().unwrap().pop();
            if let Some(task) = task {
                task();
            }
        }
    }

    static EXECUTOR: LazyLock<Executor> = LazyLock::new(Executor::new);

    #[test]
    fn test_data() {
        EXECUTOR.spawn(foo);
        EXECUTOR.spawn(foo);
        EXECUTOR.run();

        let data1 = DATA1.lock().unwrap().clone();
        let data2 = DATA2.lock().unwrap().clone();
        assert_eq!(data1.as_slice(), &[0, 1]);
        assert_eq!(data2.as_slice(), &[1, 0]);
    }
}
