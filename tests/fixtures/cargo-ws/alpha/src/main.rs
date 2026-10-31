// Prints one line per TCP connection on $PORT, so a test can trigger output on demand.
fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3999".into());
    let listener = std::net::TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    println!("alpha listening on {port}");
    for (n, _) in listener.incoming().enumerate() {
        println!("connection {n}");
    }
}
