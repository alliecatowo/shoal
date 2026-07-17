//! Full in-process Unix-socket request/response measurements.
//!
//! Unlike a codec-only microbenchmark, these cases cross newline framing,
//! JSON decoding, kernel dispatch, syntax services, response projection, and
//! response framing. Inputs and case names are stable for saved Criterion
//! baseline comparisons.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serde_json::json;
use shoal_kernel::Kernel;
use shoal_proto::{JSONRPC, Request, Response, write_frame};
use std::hint::black_box;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;

struct Connection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    worker: Option<JoinHandle<()>>,
    next_id: u64,
}

impl Connection {
    fn new() -> Self {
        let kernel = Kernel::new();
        let (client, server) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || kernel.handle_stream(server).unwrap());
        let reader = BufReader::new(client.try_clone().unwrap());
        let mut connection = Self {
            writer: client,
            reader,
            worker: Some(worker),
            next_id: 1,
        };
        let attached = connection.request(
            "session.attach",
            json!({
                "session": "criterion",
                "local_auth": "local-human"
            }),
        );
        assert!(attached.error.is_none());
        connection
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(
            &mut self.writer,
            &Request {
                jsonrpc: JSONRPC.into(),
                id: json!(id),
                method: method.into(),
                params,
            },
        )
        .unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(response.id, json!(id));
        response
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn bench_protocol(c: &mut Criterion) {
    let statement = "let value = [1, 2, 3, 4].map(x => x * 2)\n";
    let source = statement.repeat(32);
    let mut connection = Connection::new();
    let mut group = c.benchmark_group("kernel_protocol");
    group.sample_size(30);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.throughput(Throughput::Bytes(source.len() as u64));
    group.bench_function("parse_roundtrip_1kb", |b| {
        b.iter(|| {
            let response = connection.request("parse", json!({"src": black_box(&source)}));
            assert!(response.error.is_none());
            black_box(response)
        })
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("complete_roundtrip", |b| {
        b.iter(|| {
            let response = connection.request(
                "complete",
                json!({"src": "let deployment = 1\ndep", "cursor": 29}),
            );
            assert!(response.error.is_none());
            black_box(response)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_protocol);
criterion_main!(benches);
