// Memory-hungry tests for checking memory scheduling: each holds ~200 MiB for a second.
fn hold_memory() {
    let mut v = vec![0u8; 200 << 20];
    for i in (0..v.len()).step_by(4096) {
        v[i] = 1; // touch every page so it is really resident
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(v[4096], 1);
}

#[test] #[ignore = "memory scheduling demo"] fn heavy_1() { hold_memory() }
#[test] #[ignore = "memory scheduling demo"] fn heavy_2() { hold_memory() }
#[test] #[ignore = "memory scheduling demo"] fn heavy_3() { hold_memory() }
#[test] #[ignore = "memory scheduling demo"] fn heavy_4() { hold_memory() }
#[test] #[ignore = "memory scheduling demo"] fn heavy_5() { hold_memory() }
#[test] #[ignore = "memory scheduling demo"] fn heavy_6() { hold_memory() }
