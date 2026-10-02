use std::ffi::OsStr;
use std::io::Read;
use zeroize::Zeroizing;

const HELP: &str = "Shoal secret store

Usage:
  shoal-secret set NAME < VALUE
  shoal-secret list
  shoal-secret delete NAME

Commands:
  set     Store exact bytes read from stdin under NAME
  list    Print secret names, never values
  delete  Remove NAME from the encrypted store

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Output:
  list writes one name per line; set and delete are silent. Secret values are never printed.

Errors:
  Rejects invalid names, oversized stdin, malformed invocations, and store or encryption failures.

Examples:
  printf %s \"$API_TOKEN\" | shoal-secret set api-token
  shoal-secret list
  shoal-secret delete api-token

Exit status:
  0 on success; 1 for store operations; 2 for invalid invocation or store initialization.";

const SET_HELP: &str = "Store a Shoal secret

Usage:
  shoal-secret set NAME < VALUE

Options:
  -h, --help  Print this action help and exit

Output:
  Silent on success; the exact stdin bytes are never echoed.

Errors:
  Rejects invalid names, oversized input, and store or encryption failures.

Examples:
  printf %s \"$API_TOKEN\" | shoal-secret set api-token

Exit status:
  0 on success; 1 for store failures; 2 when the store cannot be opened.";

const LIST_HELP: &str = "List Shoal secret names

Usage:
  shoal-secret list

Options:
  -h, --help  Print this action help and exit

Output:
  Writes one secret name per line; values are never decrypted for output.

Errors:
  Reports store read failures without printing secret bytes.

Examples:
  shoal-secret list

Exit status:
  0 on success; 1 for store failures; 2 when the store cannot be opened.";

const DELETE_HELP: &str = "Delete a Shoal secret

Usage:
  shoal-secret delete NAME

Options:
  -h, --help  Print this action help and exit

Output:
  Silent on success.

Errors:
  Rejects invalid names and reports store update failures.

Examples:
  shoal-secret delete api-token

Exit status:
  0 on success; 1 for store failures; 2 when the store cannot be opened.";

const ACTIONS: &[&str] = &["set", "list", "delete"];

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
        println!("{HELP}");
        return;
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-secret {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let [command, flag] = args.as_slice()
        && (flag == "-h" || flag == "--help")
    {
        let help = match command.to_str() {
            Some("set") => SET_HELP,
            Some("list") => LIST_HELP,
            Some("delete") => DELETE_HELP,
            _ => {
                eprintln!("shoal-secret: unknown command (try --help)");
                std::process::exit(2)
            }
        };
        println!("{help}");
        return;
    }
    if args.iter().any(|argument| {
        matches!(
            argument.to_str(),
            Some("-h" | "--help" | "-V" | "--version")
        )
    }) {
        eprintln!("shoal-secret: help/version must be used alone (try --help)");
        std::process::exit(2);
    }
    if !args
        .first()
        .and_then(|argument| argument.to_str())
        .is_some_and(|command| ACTIONS.contains(&command))
    {
        eprintln!("shoal-secret: unknown or missing command (try --help)");
        std::process::exit(2);
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
    fn action_registry_matches_help_and_man() {
        let man = include_str!("../../../man/shoal-secret.1");
        for action in ACTIONS {
            assert!(HELP.contains(action), "root help omitted {action}");
            assert!(man.contains(action), "man page omitted {action}");
        }
    }

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
