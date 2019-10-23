#![feature(box_syntax, box_patterns)]
#![feature(nll)]
#![feature(allow_fail)]

#[macro_use]
extern crate clap;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate slog;

mod backend;
mod convert;
mod referred_tables;
mod rewrite;
mod schema;
mod utils;

use crate::backend::NoriaBackend;
use msql_srv::MysqlIntermediary;
use nom_sql::SelectStatement;
use noria::consensus::{Authority, LocalAuthority, ZookeeperAuthority};
use noria::{ControllerDescriptor, SyncControllerHandle};
use serde_json;
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter};
use std::sync::atomic::{self, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::thread;
use tokio::prelude::*;

// Just give me a damn terminal logger
// Duplicated from distributary, as the API subcrate doesn't export it.
pub fn logger_pls() -> slog::Logger {
    use slog::Drain;
    use slog::Logger;
    use slog_term::term_full;
    use std::sync::Mutex;
    Logger::root(Mutex::new(term_full()).fuse(), o!())
}

fn main() {
    use clap::{App, Arg};

    let matches = App::new("distributary-mysql")
        .version("0.0.1")
        .about("MySQL shim for Noria.")
        .arg(
            Arg::with_name("deployment")
                .long("deployment")
                .takes_value(true)
                .required(true)
                .help("Noria deployment ID to attach to."),
        )
        .arg(
            Arg::with_name("zk_addr")
                .long("zookeeper-address")
                .short("z")
                .help("IP:PORT for Zookeeper. Defaults to 127.0.0.1:2181 if neither this nor server-address is set."),
        )
        .arg(
            Arg::with_name("port")
                .long("port")
                .short("p")
                .default_value("3306")
                .takes_value(true)
                .help("Port to listen on."),
        )
        .arg(
            Arg::with_name("server_addr")
                .long("server-address")
                .short("h")
                .takes_value(true)
                .required_unless("zk_addr")
                .conflicts_with("zk_addr")
                .help("IP:PORT for the Noria Server.  Either this ore zookeeper-address is required"),
        )
        .arg(
            Arg::with_name("slowlog")
                .long("log-slow")
                .help("Log slow queries (> 5ms)"),
        )
        .arg(
            Arg::with_name("trace")
                .long("trace")
                .takes_value(true)
                .help("Trace client-side execution of every Nth operation"),
        )
        .arg(
            Arg::with_name("no-static-responses")
                .long("no-static-responses")
                .takes_value(false)
                .help("Disable checking for queries requiring static responses. Improves latency."),
        )
        .arg(
            Arg::with_name("no-sanitize")
                .long("no-sanitize")
                .takes_value(false)
                .help("Disable query sanitization. Improves latency."),
        )
        .arg(Arg::with_name("verbose").long("verbose").short("v"))
        .get_matches();

    let deployment = matches.value_of("deployment").unwrap().to_owned();
    assert!(!deployment.contains("-"));

    let port = value_t_or_exit!(matches, "port", u16);
    let trace_every = if matches.is_present("trace") {
        Some(value_t_or_exit!(matches, "trace", usize))
    } else {
        None
    };
    let slowlog = matches.is_present("slowlog");
    let sanitize = !matches.is_present("no-sanitize");
    let static_responses = !matches.is_present("no-static-responses");

    let log = logger_pls();

    info!(log, "listening on port {}", port);

    debug!(log, "Connecting to Noria...",);
    let s = tracing_subscriber::fmt::format::Format::default()
        .with_timer(tracing_subscriber::fmt::time::Uptime::default());
    let s = tracing_subscriber::FmtSubscriber::builder()
        .on_event(s)
        .finish();
    let tracer = tracing::Dispatch::new(s);
    let rt = tracing::dispatcher::with_default(&tracer, tokio::runtime::Runtime::new).unwrap();

    debug!(log, "Connected!");

    match (matches.value_of("zk_addr"), matches.value_of("server_addr")) {
        (None, Some(addr)) => {
            let lcl_auth = LocalAuthority::new();
            let saddr = addr.parse().unwrap();
            let cd = ControllerDescriptor {
                external_addr: saddr,
                worker_addr: saddr,
                domain_addr: saddr,
                nonce: 0,
            };
            let descriptor_bytes = serde_json::to_vec(&cd).unwrap();
            lcl_auth.become_leader(descriptor_bytes).unwrap();
            let ch = SyncControllerHandle::new(lcl_auth, rt.executor()).unwrap();
            run(
                rt,
                ch,
                log.clone(),
                port,
                slowlog,
                static_responses,
                sanitize,
                trace_every,
            )
        }
        (maybe_addr, None) => {
            let addr = maybe_addr.unwrap_or("127.0.0.1:2181");
            let mut zk_auth = ZookeeperAuthority::new(&format!("{}/{}", addr, deployment)).unwrap();
            zk_auth.log_with(log.clone());
            let ch = SyncControllerHandle::new(zk_auth, rt.executor()).unwrap();
            run(
                rt,
                ch,
                log.clone(),
                port,
                slowlog,
                static_responses,
                sanitize,
                trace_every,
            )
        }
        (Some(_), Some(_)) => unreachable!(),
    };
}

fn run<A, E>(
    mut rt: tokio::runtime::Runtime,
    ch: SyncControllerHandle<A, E>,
    log: slog::Logger,
    port: u16,
    slowlog: bool,
    static_responses: bool,
    sanitize: bool,
    trace_every: Option<usize>,
) where
    A: Authority + 'static,
    E: tokio::executor::Executor + Clone + Send + 'static,
{
    let listener = tokio::net::tcp::TcpListener::bind(&std::net::SocketAddr::new(
        std::net::Ipv4Addr::LOCALHOST.into(),
        port,
    ))
    .unwrap();

    let auto_increments: Arc<RwLock<HashMap<String, AtomicUsize>>> = Arc::default();
    let query_cache: Arc<RwLock<HashMap<SelectStatement, String>>> = Arc::default();

    let ctrlc = rt.block_on(future::lazy(tokio_signal::ctrl_c)).unwrap();
    let mut listener = listener.incoming().select(ctrlc.then(|r| match r {
        Ok(_) => Err(io::Error::new(io::ErrorKind::Interrupted, "got ctrl-c")),
        Err(e) => Err(e),
    }));
    let primed = Arc::new(atomic::AtomicBool::new(false));
    let ops = Arc::new(atomic::AtomicUsize::new(0));

    let mut threads = Vec::new();
    let mut i = 0;
    while let Ok((Some(s), l)) = rt.block_on(listener.into_future()) {
        listener = l;

        // one day, when msql-srv is async, this won't be necessary
        let s = {
            use std::os::unix::io::AsRawFd;
            use std::os::unix::io::FromRawFd;
            let s2 = unsafe { std::net::TcpStream::from_raw_fd(s.as_raw_fd()) };
            std::mem::forget(s); // don't drop, which would close
            s2.set_nonblocking(false).unwrap();
            s2
        };
        s.set_nodelay(true).unwrap();

        let builder = thread::Builder::new().name(format!("conn-{}", i));

        let (auto_increments, query_cache, log, primed) = (
            auto_increments.clone(),
            query_cache.clone(),
            log.clone(),
            primed.clone(),
        );

        let ch = ch.clone();
        let ops = ops.clone();

        let jh = builder
            .spawn(move || {
                let mut b = NoriaBackend::new(
                    ch,
                    auto_increments,
                    query_cache,
                    (ops, trace_every),
                    primed,
                    slowlog,
                    static_responses,
                    sanitize,
                    log,
                );
                let rs = s.try_clone().unwrap();
                if let Err(e) =
                    MysqlIntermediary::run_on(&mut b, BufReader::new(rs), BufWriter::new(s))
                {
                    match e.kind() {
                        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => {}
                        _ => {
                            panic!("{:?}", e);
                        }
                    }
                }
            })
            .unwrap();
        threads.push(jh);
        i += 1;
    }

    drop(ch);
    info!(log, "Exiting...");

    for t in threads.drain(..) {
        t.join()
            .map_err(|e| e.downcast::<io::Error>().unwrap())
            .unwrap();
    }

    rt.shutdown_on_idle().wait().unwrap();
}
