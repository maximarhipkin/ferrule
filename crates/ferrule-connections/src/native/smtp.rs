//! Just enough SMTP to send one message: EHLO, AUTH PLAIN, MAIL, RCPT,
//! DATA, QUIT, over implicit TLS (465).

use super::net::{Stream, TIMEOUT};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct Smtp {
    io: BufReader<Box<dyn Stream>>,
}

/// Why sending failed, plainly; `auth` when the login was refused.
#[derive(Debug)]
pub struct Error {
    pub text: String,
    pub auth: bool,
}

fn broken() -> Error {
    Error {
        text: "the connection to the mail server broke".into(),
        auth: false,
    }
}

impl Smtp {
    pub async fn start(stream: Box<dyn Stream>) -> Result<Self, Error> {
        let mut me = Self {
            io: BufReader::new(stream),
        };
        me.expect(220).await?;
        me.send("EHLO ferrule").await?;
        me.expect(250).await?;
        Ok(me)
    }

    async fn send(&mut self, line: &str) -> Result<(), Error> {
        let w = self.io.get_mut();
        tokio::time::timeout(TIMEOUT, async {
            w.write_all(line.as_bytes()).await?;
            w.write_all(b"\r\n").await?;
            w.flush().await
        })
        .await
        .map_err(|_| broken())?
        .map_err(|_| broken())
    }

    /// Reads a (possibly multi-line) reply: its code and last line's text.
    async fn reply(&mut self) -> Result<(u16, String), Error> {
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(TIMEOUT, self.io.read_line(&mut line))
                .await
                .map_err(|_| broken())?
                .map_err(|_| broken())?;
            if n == 0 || line.len() < 3 {
                return Err(broken());
            }
            let code: u16 = line[..3].parse().map_err(|_| broken())?;
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, line.get(4..).unwrap_or("").trim().to_string()));
            }
        }
    }

    async fn expect(&mut self, want: u16) -> Result<String, Error> {
        let (code, text) = self.reply().await?;
        if code == want {
            Ok(text)
        } else {
            Err(Error {
                auth: code == 535 || code == 534,
                text: format!("the mail server refused ({code})"),
            })
        }
    }

    pub async fn login(&mut self, user: &str, password: &str) -> Result<(), Error> {
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{password}"));
        self.send(&format!("AUTH PLAIN {token}")).await?;
        self.expect(235).await.map(|_| ())
    }

    /// Sends `message` (CRLF lines) from `from` to every `to`.
    pub async fn send_mail(
        &mut self,
        from: &str,
        to: &[String],
        message: &str,
    ) -> Result<(), Error> {
        self.send(&format!("MAIL FROM:<{from}>")).await?;
        self.expect(250).await?;
        for rcpt in to {
            self.send(&format!("RCPT TO:<{rcpt}>")).await?;
            self.expect(250).await.map_err(|e| Error {
                text: format!("the mail server wouldn't send to {rcpt}"),
                ..e
            })?;
        }
        self.send("DATA").await?;
        self.expect(354).await?;
        let mut body = String::with_capacity(message.len() + 8);
        for line in message.trim_end_matches("\r\n").split("\r\n") {
            if line.starts_with('.') {
                body.push('.');
            }
            body.push_str(line);
            body.push_str("\r\n");
        }
        body.push('.');
        self.send(&body).await?;
        self.expect(250).await?;
        Ok(())
    }

    pub async fn quit(mut self) {
        let _ = self.send("QUIT").await;
    }
}
