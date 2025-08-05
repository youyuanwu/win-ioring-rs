use crate::io_ring::IoRing;

async fn readme_test_async() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let io_ring = IoRing::builder().build().unwrap();
            let driver = crate::runtime::Driver::new(io_ring);
            let mut h_ring = driver.handle();

            let driver = tokio::task::spawn_local(async move {
                // Drive the IO operations
                driver.drive().await;
            });

            let file =
                crate::file::File::from_std(std::fs::File::open("README.md").expect("cannot open"));

            let (hr, buffer) = h_ring
                .build_read_file(&file, vec![0_u8; 20], 20, 0)
                .unwrap()
                .await;
            hr.unwrap();
            println!("Read: {:?}", String::from_utf8_lossy(&buffer));

            h_ring.shutdown();
            driver.await.unwrap();
        })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn tokio_readme_test() {
    readme_test_async().await;
}

// Custom runtime test.
#[test]
fn rt_readme_test() {
    crate::runtime::rt::Runtime::new().block_on(readme_test_async());
}
