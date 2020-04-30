use termion::{clear};
use termion::raw::IntoRawMode;
use termion::input::TermRead;

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::os::unix::net::UnixStream;
use std::os::unix::io::FromRawFd;

use mio::{Interest, Events, Poll, Token};
use mio::unix::SourceFd;
use std::os::unix::io::AsRawFd;

const STDIN_EV: Token = Token(0);
const CHOUT_EV: Token = Token(1);
const CHIN_EV: Token = Token(2);

macro_rules! log {
    ($out: ident, $fmt: literal) => {
	write!($out, concat!($fmt, "\r\n"))
    };
    ($out: ident, $fmt: literal, $($arg: expr),+) => {
	write!($out, concat!($fmt, "\r\n"), $($arg),*)
    }
}

fn handle_read<W: Write, R: Read>(o: &mut W, i: &mut R, name: &str, raw: bool) -> io::Result<(bool, String)> {
    let mut recvdata = Vec::with_capacity(1024);
    let mut connopen = true;
    loop {
	let mut buf = [0; 1024];
	match i.read(&mut buf) {
	    Ok(0) => {
		connopen = false;
		break;
	    },
	    Ok(n) => if raw {
		match buf[n-1] {
		    b'\r' => break,
		    0x18u8 => {
			connopen = false;
			break;
		    },
		    _ => {
			recvdata.extend_from_slice(&buf[..n]);
		    }
		}
	    } else {
		recvdata.extend_from_slice(&buf[..n]);
		log!(o, "-- {}: {:?}", name, recvdata)?;
		if buf[n-1] == b'\n' {
		    break;
		}
	    },
	    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
	    Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
	    Err(err) => return Err(err),
	}
    }
 
    match std::str::from_utf8(&recvdata) {
	Ok(recvstr) => {
	    Ok((connopen, recvstr.into()))
	},
	Err(_) => {
	    log!(o, "{} sent garbage: {:?}", name, recvdata)?;
	    Err(std::io::Error::new(std::io::ErrorKind::Other, "non-utf8 input"))
	}
    }
}

fn handle_write<W: Write, W2: Write>(o: &mut W, i: &mut W2, _name: &str, sendstr: &str) -> io::Result<bool> {
    let senddata: Vec<u8> = sendstr.into();
    let mut written = 0;
    let mut connopen = true;
    loop {
	match i.write(&senddata[written..]) {
	    Ok(0) => {
		connopen = false;
		break;
	    },
	    Ok(n) => written += n,
	    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
	    Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
	    Err(err) => return Err(err),
	}

	if senddata.len() == written {
	    break;
	}
    }

    Ok(connopen)
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let stdout = stdout.lock();
    let mut stdout = stdout.into_raw_mode()?;

    let mut poll = Poll::new()?;

    poll.registry().register(
	&mut SourceFd(&stdin.as_raw_fd()),
	STDIN_EV,
	Interest::READABLE)?;

    let mut child = Command::new("sh")
	.arg("-c")
	.arg("echo begin; read name; echo hi $name; echo end")
	.stdin(Stdio::piped())
	.stdout(Stdio::piped())
	.spawn()?;
    let chout = child.stdout.as_mut().unwrap();
    let chin = child.stdin.as_mut().unwrap();
    /* this is giving os errors
    unsafe {
	UnixStream::from_raw_fd(chout.as_raw_fd()).set_nonblocking(true)?;
	UnixStream::from_raw_fd(chin.as_raw_fd()).set_nonblocking(true)?;
    }*/
    poll.registry().register(
	&mut SourceFd(&chout.as_raw_fd()),
	CHOUT_EV,
	Interest::READABLE)?;
    poll.registry().register(
	&mut SourceFd(&chin.as_raw_fd()),
	CHIN_EV,
	Interest::WRITABLE)?;

    let mut stdinopen = true;
    let mut choutopen = true;
    let mut chinopen = true;
    let mut chinwaiting = false;
    let mut choutwaiting = false;
    let mut sendstr: Option<String> = None;

    let mut events = Events::with_capacity(1024);
    'poll: loop {
	poll.poll(&mut events, None)?;

	for event in &events {
	    match (event.token(), event.is_readable(), event.is_writable()) {
		(STDIN_EV, true, _) if stdinopen => {
		    log!(stdout, "-- stdin is readable")?;
		    let (connopen, recvstr) = handle_read(&mut stdout, &mut stdin, "stdin", true)?;
		    stdinopen = connopen;
		    if sendstr.is_none() {
			sendstr.replace("".into());
		    }
		    sendstr = sendstr.map(|mut s| { s.push_str(&recvstr); s });
		    if !stdinopen {
			log!(stdout, "-- stdin closed")?;
			break 'poll;
		    }
		},
		(CHOUT_EV, true, _) if choutopen => {
		    log!(stdout, "-- chout is readable")?;
		    choutwaiting = true;
		},
		(CHIN_EV, _, true) if chinopen => {
		    log!(stdout, "-- chin is writable")?;
		    chinwaiting = true;
		},
		(_, _, _) => {}
	    }

	    if chinwaiting && sendstr.is_some() {
		let sendstr = sendstr.take().unwrap();
		log!(stdout, "-- send {} to chin", sendstr)?;
		chinopen = handle_write(&mut stdout, chin, "chin", &sendstr)?;
		chin.flush().unwrap();
		if !chinopen {
		    log!(stdout, "-- chin closed")?;
		}
		chinwaiting = false;
	    }

	    if !chinwaiting && choutwaiting {
		log!(stdout, "-- chout waiting")?;
		let (connopen, recvstr) = handle_read(&mut stdout, chout, "chout", false)?;
		choutopen = connopen;
		choutwaiting = choutopen;
		log!(stdout, "chout: {}", recvstr)?;
		if !choutopen {
		    log!(stdout, "-- chout closed")?;
		}
	    }

	    if !choutopen && !chinopen {
		break 'poll;
	    }
	}
    }

    log!(stdout, "all done")?;
    child.kill()?;
    Ok(())
}
