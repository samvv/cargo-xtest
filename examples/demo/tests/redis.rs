// Talks to the `redis` service fixture (see .config/xtest.toml).
use std::io::{Read, Write};

#[test]
#[ignore = "needs the redis fixture; run with `cargo xtest`"]
fn pings_redis() {
    let addr = std::env::var("REDIS_ADDR").expect("REDIS_ADDR");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(b"PING\r\n").unwrap();
    let mut buf = [0u8; 7];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"+PONG\r\n");
}
