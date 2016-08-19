use std::borrow::Cow;
use std::io;
use data_encoding::base64;
use rand::distributions::IndependentSample;
use rand::distributions::range::Range;
use rand::os::OsRng;
use rand::Rng;
use ring::digest::{digest, SHA256, Digest};
use ring::hmac::{SigningKey, SigningContext, sign};
use ring::pbkdf2::{HMAC_SHA256, derive};
use error::{Error, Kind, Field};
use super::DebugDigest;

/// The length of the client nonce in characters/bytes.
const NONCE_LENGTH: usize = 24;
/// The length of a SHA-256 hash in bytes.
const SHA256_LEN: usize = 32;

/// Parses a `server_first_message` returning a (none, salt, iterations) tuple if successful.
fn parse_server_first(data: &str) -> Result<(&str, Vec<u8>, u16), Error> {
    if data.len() < 2 {
        return Err(Error::Protocol(Kind::ExpectedField(Field::Nonce)));
    }
    let mut parts = data.split(',').peekable();
    match parts.peek() {
        Some(part) if &part.as_bytes()[..2] == b"m=" => {
            return Err(Error::UnsupportedExtension);
        }
        Some(_) => {}
        None => {
            return Err(Error::Protocol(Kind::ExpectedField(Field::Nonce)));
        }
    }
    let nonce = match parts.next() {
        Some(part) if &part.as_bytes()[..2] == b"r=" => &part[2..],
        _ => {
            return Err(Error::Protocol(Kind::ExpectedField(Field::Nonce)));
        }
    };
    let salt = match parts.next() {
        Some(part) if &part.as_bytes()[..2] == b"s=" => {
            try!(base64::decode(part[2..].as_bytes())
                .map_err(|_| Error::Protocol(Kind::InvalidField(Field::Salt))))
        }
        _ => {
            return Err(Error::Protocol(Kind::ExpectedField(Field::Salt)));
        }
    };
    let iterations = match parts.next() {
        Some(part) if &part.as_bytes()[..2] == b"i=" => {
            try!(part[2..]
                .parse()
                .map_err(|_| Error::Protocol(Kind::InvalidField(Field::Salt))))
        }
        _ => {
            return Err(Error::Protocol(Kind::ExpectedField(Field::Iterations)));
        }
    };
    Ok((nonce, salt, iterations))
}

/// The initial state of the SCRAM mechanism. It's the entry point for a SCRAM handshake.
#[derive(Debug)]
pub struct ClientFirst<'a> {
    gs2header: Cow<'static, str>,
    password: &'a str,
    nonce: String,
    authcid: &'a str,
}

impl<'a> ClientFirst<'a> {
    /// Constructs an initial state for the SCRAM mechanism using the provided credentials.
    ///
    /// # Arguments
    ///
    /// * authcid - An username used for authentication.
    /// * password - A password used to prove that the user is authentic.
    /// * authzid - An username used for authorization. This can be used to impersonate as `authzid`
    /// using the credentials of `authcid`. If `authzid` is `None` the authorized username will be
    /// the same as the authenticated username.
    ///
    /// # Return value
    ///
    /// An I/O error is returned if the internal random number generator couldn't be constructed.
    pub fn new(authcid: &'a str, password: &'a str, authzid: Option<&'a str>) -> io::Result<Self> {
        let rng = try!(OsRng::new());
        Ok(Self::with_rng(authcid, password, authzid, rng))
    }

    /// Constructs an initial state for the SCRAM mechanism using the provided credentials and a
    /// custom random number generator.
    ///
    /// # Arguments
    ///
    /// * authcid - An username used for authentication.
    /// * password - A password used to prove that the user is authentic.
    /// * authzid - An username used for authorization. This can be used to impersonate as `authzid`
    /// using the credentials of `authcid`. If `authzid` is `None` the authorized username will be
    /// the same as the authenticated username.
    /// * rng: A random number generator used to generate random nonces. Please only use a
    /// cryptographically secure random number generator!
    pub fn with_rng<R: Rng>(authcid: &'a str,
                            password: &'a str,
                            authzid: Option<&'a str>,
                            mut rng: R)
                            -> Self {
        let gs2header: Cow<'static, str> = match authzid {
            Some(authzid) => format!("n,a={},", authzid).into(),
            None => "n,,".into(),
        };
        let range = Range::new(33, 125);
        let nonce: String = (0..NONCE_LENGTH)
            .map(move |_| {
                let x: u8 = range.ind_sample(&mut rng);
                if x > 43 {
                    (x + 1) as char
                } else {
                    x as char
                }
            })
            .collect();

        ClientFirst {
            gs2header: gs2header,
            password: password,
            authcid: authcid,
            nonce: nonce,
        }
    }

    /// Returns the next state and the first client message.
    ///
    /// Call the
    /// [`ServerFirst::handle_server_first`](struct.ServerFirst.html#method.handle_server_first)
    /// method to continue the SCRAM handshake.
    pub fn client_first(self) -> (ServerFirst<'a>, String) {
        let escaped_authcid: Cow<'a, str> =
            if self.authcid.chars().any(|chr| chr == ',' || chr == '=') {
                self.authcid.into()
            } else {
                self.authcid.replace(',', "=2C").replace('=', "=3D").into()
            };
        let client_first_bare = format!("n={},r={}", escaped_authcid, self.nonce);
        let client_first = format!("{}{}", self.gs2header, client_first_bare);
        let server_first = ServerFirst {
            gs2header: self.gs2header,
            password: self.password,
            client_nonce: self.nonce,
            client_first_bare: client_first_bare,
        };
        (server_first, client_first)
    }
}

/// The second state of the SCRAM mechanism after the first client message was computed.
#[derive(Debug)]
pub struct ServerFirst<'a> {
    gs2header: Cow<'static, str>,
    password: &'a str,
    client_nonce: String,
    client_first_bare: String,
}

impl<'a> ServerFirst<'a> {
    /// Processes the first answer from the server and returns the next state or an error. If an
    /// error is returned the SCRAM handshake is aborted.
    ///
    /// Call the [`ClientFinal::client_final`](struct.ClientFinal.html#method.client_final) method
    /// to continue the handshake.
    ///
    /// # Return value
    ///
    /// This method returns only a subset of the errors defined in [`Error`](enum.Error.html):
    ///
    /// * Error::Protocol
    /// * Error::UnsupportedExtension
    pub fn handle_server_first(self, server_first: &str) -> Result<ClientFinal, Error> {
        fn sign_slice(key: &SigningKey, slice: &[&[u8]]) -> Digest {
            let mut signature_context = SigningContext::with_key(key);
            for item in slice {
                signature_context.update(item);
            }
            signature_context.sign()
        }

        let (nonce, salt, iterations) = try!(parse_server_first(server_first));
        if !nonce.starts_with(&self.client_nonce) {
            return Err(Error::Protocol(Kind::InvalidNonce));
        }

        let client_final_without_proof = format!("c={},r={}",
                                                 base64::encode(self.gs2header.as_bytes()),
                                                 nonce);
        let auth_message = [self.client_first_bare.as_bytes(),
                            b",",
                            server_first.as_bytes(),
                            b",",
                            client_final_without_proof.as_bytes()];

        let mut salted_password = [0u8; SHA256_LEN];
        derive(&HMAC_SHA256,
               iterations as usize,
               &salt,
               self.password.as_bytes(),
               &mut salted_password);
        let salted_password_signing_key = SigningKey::new(&SHA256, &salted_password);
        let client_key = sign(&salted_password_signing_key, b"Client Key");
        let server_key = sign(&salted_password_signing_key, b"Server Key");
        let stored_key = digest(&SHA256, client_key.as_ref());
        let stored_key_signing_key = SigningKey::new(&SHA256, stored_key.as_ref());
        let client_signature = sign_slice(&stored_key_signing_key, &auth_message);
        let server_signature_signing_key = SigningKey::new(&SHA256, server_key.as_ref());
        let server_signature = sign_slice(&server_signature_signing_key, &auth_message);
        let mut client_proof = [0u8; SHA256_LEN];
        let xor_iter =
            client_key.as_ref().iter().zip(client_signature.as_ref()).map(|(k, s)| k ^ s);
        for (p, x) in client_proof.iter_mut().zip(xor_iter) {
            *p = x
        }

        let client_final = format!("c={},r={},p={}",
                                   base64::encode(self.gs2header.as_bytes()),
                                   nonce,
                                   base64::encode(&client_proof));
        Ok(ClientFinal {
            server_signature: DebugDigest(server_signature),
            client_final: client_final,
        })
    }
}

/// The third state of the SCRAM mechanism after the first server message was successfully
/// processed.
#[derive(Debug)]
pub struct ClientFinal {
    server_signature: DebugDigest,
    client_final: String,
}

impl ClientFinal {
    /// Returns the next state and the final client message.
    ///
    /// Call the
    /// [`ServerFinal::handle_server_final`](struct.ServerFinal.html#method.handle_server_final)
    /// method to continue the SCRAM handshake.
    #[inline]
    pub fn client_final(self) -> (ServerFinal, String) {
        let server_final = ServerFinal { server_signature: self.server_signature };
        (server_final, self.client_final)
    }
}

/// The final state of the SCRAM mechanism after the final client message was computed.
#[derive(Debug)]
pub struct ServerFinal {
    server_signature: DebugDigest,
}

impl ServerFinal {
    /// Processes the final answer from the server and returns the authentication result.
    ///
    /// # Return value
    ///
    /// * A value of `Ok(())` signals a successful authentication attempt.
    /// * A value of `Err(Error::Protocol(_)` or `Err(Error::UnsupportedExtension)` means that the
    /// authentication request failed.
    /// * A value of `Err(Error::InvalidServer)` or `Err(Error::Authentication(_))` means that the
    /// authentication request was rejected.
    ///
    /// Detailed semantics are documented in the [`Error`](enum.Error.html) type.
    pub fn handle_server_final(self, server_final: &str) -> Result<(), Error> {
        if server_final.len() < 2 {
            return Err(Error::Protocol(Kind::ExpectedField(Field::VerifyOrError)));
        }
        match &server_final[..2] {
            "v=" => {
                let verifier = try!(base64::decode(&server_final.as_bytes()[2..])
                    .map_err(|_| Error::Protocol(Kind::InvalidField(Field::VerifyOrError))));
                if self.server_signature.as_ref() == &*verifier {
                    Ok(())
                } else {
                    Err(Error::InvalidServer)
                }
            }
            "e=" => Err(Error::Authentication(server_final[2..].to_string())),
            _ => Err(Error::Protocol(Kind::ExpectedField(Field::VerifyOrError))),
        }
    }
}
