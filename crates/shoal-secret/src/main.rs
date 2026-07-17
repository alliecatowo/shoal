use std::io::Read;
fn main() {
    let dir = shoal_paths::ShoalPaths::discover()
        .data_dir()
        .join("secrets");
    let store = match shoal_secret::SecretStore::open(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("shoal-secret: {e}");
            std::process::exit(2)
        }
    };
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let r = match args.as_slice() {
        [cmd] if cmd == "list" => store.list().map(|v| {
            for n in v {
                println!("{n}")
            }
        }),
        [cmd, name] if cmd == "set" => {
            let mut value = Vec::new();
            std::io::stdin()
                .read_to_end(&mut value)
                .and_then(|_| store.set(name, &value))
        }
        [cmd, name] if cmd == "delete" => store.delete(name).map(|_| ()),
        _ => {
            eprintln!("usage: shoal-secret set NAME < value | list | delete NAME");
            std::process::exit(2)
        }
    };
    if let Err(e) = r {
        eprintln!("shoal-secret: {e}");
        std::process::exit(1)
    }
}
