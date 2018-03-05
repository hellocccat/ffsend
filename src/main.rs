extern crate base64;
extern crate crypto;
extern crate hyper;
extern crate mime_guess;
extern crate rand;
extern crate reqwest;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crypto::aead::AeadEncryptor;
use crypto::aes::KeySize;
use crypto::aes_gcm::AesGcm;
use crypto::digest::Digest;
use crypto::hkdf::{hkdf_extract, hkdf_expand};
use crypto::sha2::Sha256;
use hyper::error::Error as HyperError;
use mime_guess::Mime;
use rand::{Rng, thread_rng};
use reqwest::header::{
    Authorization,
    Formatter as HeaderFormatter,
    Header,
    Raw
};
use reqwest::mime::APPLICATION_OCTET_STREAM;
use reqwest::multipart::Part;

fn main() {
    // TODO: a fixed path for now, as upload test
    let path = Path::new("/home/timvisee/Pictures/Avatar/1024x1024/Avatar.png");
    let file_ext = path.extension().unwrap().to_str().unwrap();
    let file_name = path.file_name().unwrap().to_str().unwrap().to_owned();

    // Create a new reqwest client
    let client = reqwest::Client::new();

    // Generate a secret and iv
    let mut secret = [0u8; 16];
    let mut iv = [0u8; 12];
    thread_rng().fill_bytes(&mut secret);
    thread_rng().fill_bytes(&mut iv);

    // Derive keys
    let encrypt_key = derive_file_key(&secret);
    let auth_key = derive_auth_key(&secret, None, None);
    let meta_key = derive_meta_key(&secret);

    // Generate a file and meta cipher
    // TODO: use the proper key size here, and the proper aad
    let file_cipher = AesGcm::new(KeySize::KeySize128, &encrypt_key, &iv, b"");
    let mut meta_cipher = AesGcm::new(KeySize::KeySize128, &meta_key, &[0u8; 12], b"");

    // Guess the mimetype of the file
    let file_mime = mime_guess::get_mime_type(file_ext);

    // Construct the metadata
    let metadata = Metadata::from(&iv, file_name.clone(), file_mime);

    // Encrypt the metadata, append the tag
    let metadata = metadata.to_json().into_bytes();
    let mut metadata_tag = vec![0u8; 16];
    let mut metadata_encrypted = vec![0u8; metadata.len()];
    meta_cipher.encrypt(&metadata, &mut metadata_encrypted, &mut metadata_tag);
    metadata_encrypted.append(&mut metadata_tag);

    // Open the file and create an encrypted file reader
    let file = File::open(path).unwrap();
    let reader = EncryptedFileReaderTagged::new(file, file_cipher);

    // Build the file part, configure the form to send
    let part = Part::reader(reader)
        .file_name(file_name)
        .mime(APPLICATION_OCTET_STREAM);
    let form = reqwest::multipart::Form::new()
        .part("data", part);

    // Make the request
    let mut res = client.post("http://localhost:8080/api/upload")
        .header(Authorization(format!("send-v1 {}", base64::encode(&auth_key))))
        .header(XFileMetadata::from(&metadata_encrypted))
        .multipart(form)
        .send()
        .unwrap();

    let text = res.text().unwrap();

    // TODO: remove after debugging
    println!("TEXT: {}", text);
}

const TAG_LEN: usize = 16;

/// A file reader, that encrypts the file with the given cipher, and appends
/// the raw cipher tag.
///
/// This reader is lazy, and reads/encrypts the file on the fly.
struct EncryptedFileReaderTagged<'a> {
    /// The file that is being read.
    file: File,

    /// The cipher to use.
    cipher: Arc<Mutex<AesGcm<'a>>>,

    /// The crypto tag.
    tag: [u8; TAG_LEN],

    /// A tag cursor, used as reader for the appended tag.
    tag_cursor: Option<Cursor<Vec<u8>>>,
}

impl<'a: 'static> EncryptedFileReaderTagged<'a> {
    /// Construct a new reader.
    // TODO: try to borrow here
    pub fn new(file: File, cipher: AesGcm<'a>) -> Self {
        EncryptedFileReaderTagged {
            file,
            cipher: Arc::new(Mutex::new(cipher)),
            tag: [0u8; TAG_LEN],
            tag_cursor: None,
        }
    }

    /// Get the length.
    pub fn len(&self) -> Result<u64, io::Error> {
        Ok(self.file.metadata()?.len() + TAG_LEN as u64)
    }
}

impl<'a: 'static> Read for EncryptedFileReaderTagged<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        // Create a buffer with the same size, for the raw data
        let mut raw = vec![0u8; buf.len()];

        // Read from the file
        let len = self.file.read(&mut raw)?;

        // Encrypt the read data
        if len > 0 {
            // Lock the cipher mutex
            let mut cipher = self.cipher.lock().unwrap();

            // Encrypt the raw slice, put it into the user buffer
            println!("DEBUG: Tag (from): {:?}", self.tag);
            cipher.encrypt(&raw[..len], buf, &mut self.tag);
            println!("DEBUG: Tag (to): {:?}", self.tag);

            Ok(len)
        } else {
            // Initialise the tag cursor
            if self.tag_cursor.is_none() {
                self.tag_cursor = Some(Cursor::new(self.tag.to_vec()));
            }

            // Read from the tag cursor
            self.tag_cursor.as_mut().unwrap().read(buf)
        }
    }
}

// TODO: do not implement, make the reader send!
unsafe impl<'a: 'static> ::std::marker::Send for EncryptedFileReaderTagged<'a> {}

#[derive(Clone)]
struct XFileMetadata {
    /// The metadata, as a base64 encoded string.
    metadata: String,
}

impl XFileMetadata {
    pub fn new(metadata: String) -> Self {
        XFileMetadata {
            metadata,
        }
    }

    pub fn from(bytes: &[u8]) -> Self {
        XFileMetadata::new(base64::encode(bytes))
    }
}

impl Header for XFileMetadata {
    fn header_name() -> &'static str {
        "X-File-Metadata"
    }

    fn parse_header(raw: &Raw) -> Result<Self, HyperError> {
        // TODO: implement this some time
        unimplemented!();
    }

    fn fmt_header(&self, f: &mut HeaderFormatter) -> fmt::Result {
        // TODO: is this encoding base64 for us?
        f.fmt_line(&self.metadata)
    }
}

#[derive(Serialize)]
struct Metadata {
    /// The input vector
    iv: String,

    /// The file name
    name: String,

    /// The file mimetype
    #[serde(rename="type")]
    mime: String,
}

impl Metadata {
    /// Construct metadata from the given properties.
    ///
    /// Parameters:
    /// * iv: initialisation vector
    /// * name: file name
    /// * mime: file mimetype
    pub fn from(iv: &[u8], name: String, mime: Mime) -> Self {
        Metadata {
            iv: base64::encode(iv),
            name,
            mime: mime.to_string(),
        }
    }

    /// Convert this structure to a JSON string.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self).unwrap()
    }
}

fn derive_file_key(secret: &[u8]) -> Vec<u8> {
    hkdf(16, secret, None, Some(b"encryption"))
}

fn derive_auth_key(secret: &[u8], password: Option<String>, url: Option<String>) -> Vec<u8> {
    if password.is_none() {
        hkdf(64, secret, None, Some(b"authentication"))
    } else {
        // TODO: implement this
        unimplemented!();
    }
}

fn derive_meta_key(secret: &[u8]) -> Vec<u8> {
    hkdf(16, secret, None, Some(b"metadata"))
}

fn hkdf<'a>(
    length: usize,
    ikm: &[u8],
    salt: Option<&[u8]>,
    info: Option<&[u8]>
) -> Vec<u8> {
    // Get the salt and info parameters, use defaults if undefined
    let salt = salt.unwrap_or(b"");
    let info = info.unwrap_or(b"");

    // Define the digest to use
    let digest = Sha256::new();

    let mut pkr: Vec<u8> = vec![0u8; digest.output_bytes()];
    hkdf_extract(digest, salt, ikm, &mut pkr);

    let mut okm: Vec<u8> = vec![0u8; length];
    hkdf_expand(digest, &pkr, info, &mut okm);

    okm
}
