fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "rfb".into());
    println!("hello from rust-in-image, {name}!");
}
