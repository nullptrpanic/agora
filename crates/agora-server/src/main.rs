fn hello() -> &'static str {
    "hello from agora-server"
}

fn main() {
    println!("{}", hello());
}
