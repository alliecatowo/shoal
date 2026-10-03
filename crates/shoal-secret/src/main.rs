use std::ffi::OsStr;
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

fn secret_name(name: &OsStr) -> std::io::Result<&str> {
    name.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret name is not valid UTF-8",
        )
    })
}

fn main() {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        println!(
            "Shoal secret store\n\nUsage:\n  shoal-secret set NAME < VALUE\n  shoal-secret list\n  shoal-secret delete NAME"
        );
        return;
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-secret {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let dir = shoal_paths::ShoalPaths::discover()
        .secret_dir()
        .to_path_buf();
    let store = match shoal_secret::SecretStore::open(dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("shoal-secret: {e}");
            std::process::exit(2)
        }
    };
    let r = match args.as_slice() {
        [cmd] if cmd == OsStr::new("list") => store.list().map(|v| {
            for n in v {
                println!("{n}")
            }
        }),
        [cmd, name] if cmd == OsStr::new("set") => secret_name(name).and_then(|name| {
            read_secret_value(std::io::stdin().lock()).and_then(|value| store.set(name, &value))
        }),
        [cmd, name] if cmd == OsStr::new("delete") => {
            secret_name(name).and_then(|name| store.delete(name).map(|_| ()))
        }
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

    #[cfg(unix)]
    #[test]
    fn non_utf8_name_is_a_typed_input_error() {
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(
            secret_name(OsStr::from_bytes(&[0xff])).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
