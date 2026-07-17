use std::io::Read;
use zeroize::Zeroizing;

fn read_secret_value(mut reader: impl Read) -> std::io::Result<Zeroizing<Vec<u8>>> {
    let mut value = Zeroizing::new(Vec::with_capacity(8 * 1024));
    Read::by_ref(&mut reader)
        .take(shoal_secret::MAX_SECRET_VALUE_BYTES as u64 + 1)
        .read_to_end(&mut value)?;
    if value.len() > shoal_secret::MAX_SECRET_VALUE_BYTES {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret value exceeds byte limit",
        ))
    } else {
        Ok(value)
    }
}

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
            read_secret_value(std::io::stdin().lock()).and_then(|value| store.set(name, &value))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_admission_is_bounded_before_store_mutation() {
        let ordinary = read_secret_value(&b"exact bytes"[..]).unwrap();
        assert_eq!(&*ordinary, b"exact bytes");

        let hostile = vec![0u8; shoal_secret::MAX_SECRET_VALUE_BYTES + 1];
        assert_eq!(
            read_secret_value(hostile.as_slice()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
