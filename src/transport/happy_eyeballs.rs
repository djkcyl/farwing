//! TCP connect over every resolved address, in the manner of RFC 8305: the
//! families are interleaved, each further attempt starts 250 ms after the
//! previous one (or at once when it fails), and the first success wins. A
//! name whose first address is unreachable (for example `localhost` → `::1`
//! on an IPv4-only server) no longer costs a full connect timeout.
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Addresses in attempt order: the first family first, then alternating.
fn interleave(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addresses.first() else {
        return Vec::new();
    };
    let (mut primary, mut secondary): (Vec<_>, Vec<_>) = addresses
        .iter()
        .copied()
        .partition(|address| address.is_ipv6() == first.is_ipv6());
    primary.dedup();
    secondary.dedup();
    let mut ordered = Vec::with_capacity(addresses.len());
    let (mut a, mut b) = (primary.into_iter(), secondary.into_iter());
    loop {
        match (a.next(), b.next()) {
            (None, None) => break,
            (x, y) => ordered.extend(x.into_iter().chain(y)),
        }
    }
    ordered
}

pub(crate) async fn connect(addresses: &[SocketAddr]) -> io::Result<TcpStream> {
    let mut pending = interleave(addresses).into_iter();
    let mut attempts = JoinSet::new();
    let mut last_error = None;
    loop {
        if let Some(address) = pending.next() {
            attempts.spawn(async move { TcpStream::connect(address).await });
        } else if attempts.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::AddrNotAvailable, "no address to connect to")
            }));
        }
        // Wait for a result, but start the next address after the delay.
        let more = pending.len() > 0;
        let next = tokio::select! {
            result = attempts.join_next() => result,
            _ = tokio::time::sleep(ATTEMPT_DELAY), if more => continue,
        };
        match next {
            Some(Ok(Ok(stream))) => {
                attempts.abort_all();
                return Ok(stream);
            }
            Some(Ok(Err(error))) => last_error = Some(error),
            Some(Err(join)) => last_error = Some(io::Error::other(join)),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_alternate_starting_with_the_first() {
        let v6: SocketAddr = "[::1]:1".parse().unwrap();
        let v6b: SocketAddr = "[::2]:1".parse().unwrap();
        let v4: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(interleave(&[v6, v6b, v4]), vec![v6, v4, v6b]);
        assert_eq!(interleave(&[v4, v6]), vec![v4, v6]);
        assert!(interleave(&[]).is_empty());
    }

    #[tokio::test]
    async fn a_refused_first_address_falls_through_at_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        // A port that was just free is refused immediately.
        let closed = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap()
        };
        let started = std::time::Instant::now();
        connect(&[closed, open]).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(connect(&[closed]).await.is_err());
    }

    #[tokio::test]
    async fn a_silent_first_address_does_not_block_the_next() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        // TEST-NET-1 is not routed; the attempt hangs until it times out.
        let silent: SocketAddr = "192.0.2.1:9".parse().unwrap();
        let started = std::time::Instant::now();
        connect(&[silent, open]).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
