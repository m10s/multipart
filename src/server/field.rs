// Copyright 2016 `multipart` Crate Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! `multipart` field header parsing.

use super::httparse::{self, EMPTY_HEADER, Header, Status};

use super::Multipart;

use super::boundary::BoundaryReader;

use mime::Mime;

use std::io::{self, Read, BufRead, Write};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::str;

const RANDOM_FILENAME_LEN: usize = 12;

macro_rules! try_io(
    ($try:expr) => (
        {
            use std::io::{Error, ErrorKind};
            match $try {
                Ok(val) => val,
                Err(e) => return Err(Error::new(ErrorKind::InvalidData, e)),
            }
        }
    )
);

const EMPTY_STR_HEADER: StrHeader<'static> = StrHeader {
    name: "",
    val: "",
};

/// Not exposed
#[derive(Copy, Clone, Debug)]
pub struct StrHeader<'a> {
    name: &'a str,
    val: &'a str,
}

const MAX_ATTEMPTS: usize = 5;

fn with_headers<R, F, Ret>(r: &mut R, closure: F) -> io::Result<Ret>
where R: BufRead, F: FnOnce(&[StrHeader]) -> Ret {
    const HEADER_LEN: usize = 4;

    // These are only written once so they don't need to be `mut` or initialized.
    let consume;
    let ret;

    let mut attempts = 0;

    loop {
        let mut raw_headers = [EMPTY_HEADER; HEADER_LEN];

        let buf = try!(r.fill_buf());

        if attempts == MAX_ATTEMPTS {
            error!("Could not read field headers.");
            // RFC: return an actual error instead?
            return Ok(closure(&[]));
        }

        match httparse::parse_headers(buf, &mut raw_headers) {
            Ok(Status::Complete((consume_, raw_headers))) =>  {
                consume = consume_;
                let mut headers = [EMPTY_STR_HEADER; HEADER_LEN];
                let headers = try!(copy_headers(raw_headers, &mut headers));
                debug!("Parsed headers: {:?}", headers);
                ret = closure(headers);
                break;
            },
            Ok(Status::Partial) => { attempts += 1; continue },
            Err(err) => {
                error!("Error returned from parse_headers(): {}, Buf: {:?}",
                       err, String::from_utf8_lossy(buf));
                return Err(io::Error::new(io::ErrorKind::InvalidData, err));
            },
        }
    }

    r.consume(consume);

    Ok(ret)
}

fn copy_headers<'h, 'b: 'h>(raw: &[Header<'b>], headers: &'h mut [StrHeader<'b>]) -> io::Result<&'h [StrHeader<'b>]> {
    for (raw, header) in raw.iter().zip(&mut *headers) {
        header.name = raw.name;
        header.val = try!(io_str_utf8(raw.value));
    }

    Ok(&mut headers[..raw.len()])
}

/// The headers that (may) appear before a `multipart/form-data` field.
pub struct FieldHeaders {
    /// The `Content-Disposition` header, required.
    cont_disp: ContentDisp,
    /// The `Content-Type` header, optional.
    cont_type: Option<Mime>,
}

impl FieldHeaders {
    /// Parse the field headers from the passed `BufRead`, consuming the relevant bytes.
    pub fn read_from<R: BufRead>(r: &mut R) -> io::Result<Option<Self>> {
        with_headers(r, Self::parse)
    }

    fn parse(headers: &[StrHeader]) -> Option<FieldHeaders> {
        let cont_disp = try_opt!(
                ContentDisp::parse(headers),
                debug!("Failed to read Content-Disposition")
            );

        let cont_type = parse_cont_type(headers);

        Some(FieldHeaders {
            cont_disp: cont_disp,
            cont_type: cont_type,
        })
    }
}

/// The `Content-Disposition` header.
pub struct ContentDisp {
    /// The name of the `multipart/form-data` field.
    field_name: String,
    /// The optional filename for this field.
    filename: Option<String>,
}

impl ContentDisp {
    fn parse(headers: &[StrHeader]) -> Option<ContentDisp> {
        if headers.is_empty() {
            return None;
        }

        const CONT_DISP: &'static str = "Content-Disposition";

        let header = try_opt!(
            find_header(headers, CONT_DISP),
            error!("Field headers did not contain Content-Disposition header (required)")
        );

        const NAME: &'static str = "name=";
        const FILENAME: &'static str = "filename=";

        let after_disp_type = {
            let (disp_type, after_disp_type) = try_opt!(
                split_once(header.val, ';'),
                error!("Expected additional data after Content-Disposition type, got {:?}",
                header.val)
            );


            if disp_type.trim() != "form-data" {
                error!("Unexpected Content-Disposition value: {:?}", disp_type);
                return None;
            };

            after_disp_type
        };

        let (field_name, after_field_name) = try_opt!(
            get_str_after(NAME, ';', after_disp_type),
            error!("Expected field name and maybe filename, got {:?}", after_disp_type)
        );

        let field_name = trim_quotes(field_name);

        let filename = get_str_after(FILENAME, ';', after_field_name)
            .map(|(filename, _)| trim_quotes(filename).to_owned());

        Some(ContentDisp { field_name: field_name.to_owned(), filename: filename })
    }
}

fn parse_cont_type(headers: &[StrHeader]) -> Option<Mime> {
    const CONTENT_TYPE: &'static str = "Content-Type";

    let header = try_opt!(
        find_header(headers, CONTENT_TYPE),
        debug!("Content-Type header not found for field.")
    );

    // Boundary parameter will be parsed into the `Mime`
    debug!("Found Content-Type: {:?}", header.val);
    let content_type = read_content_type(header.val.trim());
    Some(content_type)
}

/// A field in a multipart request. May be either text or a binary stream (file).
#[derive(Debug)]
pub struct MultipartField<'a, B: 'a> {
    /// The field's name from the form
    pub name: String,
    /// The data of the field. Can be text or binary.
    pub data: MultipartData<'a, B>,
}

pub fn read_field<B: Read>(multipart: &mut Multipart<B>) -> io::Result<Option<MultipartField<B>>> {
    let field_headers =  match multipart.read_field_headers() {
        Ok(Some(headers)) => headers,
        Ok(None) => return Ok(None),
        Err(err) => return Err(err)
    };

    let data = match field_headers.cont_type {
        Some(content_type) => {
            MultipartData::File(
                MultipartFile::from_stream(
                    field_headers.cont_disp.filename,
                    content_type,
                    &mut multipart.source,
                )
            )
        },
        None => {
            let text = try!(multipart.read_to_string());
            MultipartData::Text(&text)
        },
    };

    Ok(Some(
        MultipartField {
            name: field_headers.cont_disp.field_name,
            data: data,
        }
    ))
}

/// The data of a field in a `multipart/form-data` request.
#[derive(Debug)]
pub enum MultipartData<'a, B: 'a> {
    /// The field's payload is a text string.
    Text(&'a str),
    /// The field's payload is a binary stream (file).
    File(MultipartFile<'a, B>),
    // TODO: Support multiple files per field (nested boundaries)
    // MultiFiles(Vec<MultipartFile>),
}

impl<'a, B> MultipartData<'a, B> {
    /// Borrow this payload as a text field, if possible.
    pub fn as_text(&self) -> Option<&str> {
        match *self {
            MultipartData::Text(ref s) => Some(s),
            _ => None,
        }
    }

    /// Borrow this payload as a file field, if possible.
    /// Mutably borrows so the contents can be read.
    pub fn as_file(&mut self) -> Option<&mut MultipartFile<'a, B>> {
        match *self {
            MultipartData::File(ref mut file) => Some(file),
            _ => None,
        }
    }
}

/// A representation of a file in HTTP `multipart/form-data`.
///
/// Note that the file is not yet saved to the local filesystem;
/// instead, this struct exposes `Read` and `BufRead` impls which point
/// to the beginning of the file's contents in the HTTP stream.
///
/// You can read it to EOF, or use one of the `save_*()` methods here
/// to save it to disk.
#[derive(Debug)]
pub struct MultipartFile<'a, B: 'a> {
    filename: Option<String>,
    content_type: Mime,
    stream: &'a mut BoundaryReader<B>,
}

impl<'a, B: Read> MultipartFile<'a, B> {
    fn from_stream(filename: Option<String>,
                   content_type: Mime,
                   stream: &'a mut BoundaryReader<B>) -> MultipartFile<'a, B> {
        MultipartFile {
            filename: filename,
            content_type: content_type,
            stream: stream,
        }
    }

    /// Save this file to the given output stream.
    ///
    /// If successful, returns the number of bytes written.
    ///
    /// Retries when `io::Error::kind() == io::ErrorKind::Interrupted`.
    pub fn save_to<W: Write>(&mut self, mut out: W) -> io::Result<u64> {
        retry_on_interrupt(|| io::copy(self.stream, &mut out))
    }

    /// Save this file to the given output stream, **truncated** to `limit`
    /// (no more than `limit` bytes will be written out).
    ///
    /// If successful, returns the number of bytes written.
    ///
    /// Retries when `io::Error::kind() == io::ErrorKind::Interrupted`.
    pub fn save_to_limited<W: Write>(&mut self, mut out: W, limit: u64) -> io::Result<u64> {
        retry_on_interrupt(|| io::copy(&mut self.stream.take(limit), &mut out))
    }

    /// Save this file to `path`.
    ///
    /// Returns the saved file info on success, or any errors otherwise.
    ///
    /// Retries when `io::Error::kind() == io::ErrorKind::Interrupted`.
    pub fn save_as<P: Into<PathBuf>>(&mut self, path: P) -> io::Result<SavedFile> {
        let path = path.into();
        let file = try!(create_full_path(&path));
        let size = try!(self.save_to(file));

        Ok(SavedFile {
            path: path,
            filename: self.filename.clone(),
            size: size,
        })
    }

    /// Save this file in the directory pointed at by `dir`,
    /// using a random alphanumeric string as the filename.
    ///
    /// Any missing directories in the `dir` path will be created.
    ///
    /// Returns the saved file's info on success, or any errors otherwise.
    ///
    /// Retries when `io::Error::kind() == io::ErrorKind::Interrupted`.
    pub fn save_in<P: AsRef<Path>>(&mut self, dir: P) -> io::Result<SavedFile> {
        let path = dir.as_ref().join(::random_alphanumeric(RANDOM_FILENAME_LEN));
        self.save_as(path)
    }

    /// Save this file to `path`, **truncated** to `limit` (no more than `limit` bytes will be written out).
    ///
    /// Any missing directories in the `dir` path will be created.
    ///
    /// Returns the saved file's info on success, or any errors otherwise.
    ///
    /// Retries when `io::Error::kind() == io::ErrorKind::Interrupted`.
    pub fn save_as_limited<P: Into<PathBuf>>(&mut self, path: P, limit: u64) -> io::Result<SavedFile> {
        let path = path.into();
        let file = try!(create_full_path(&path));
        let size = try!(self.save_to_limited(file, limit));

        Ok(SavedFile {
            path: path,
            filename: self.filename.clone(),
            size: size,
        })
    }

    /// Save this file in the directory pointed at by `dir`,
    /// using a random alphanumeric string as the filename.
    ///
    /// **Truncates** file to `limit` (no more than `limit` bytes will be written out).
    ///
    /// Any missing directories in the `dir` path will be created.
    ///
    /// Returns the saved file's info on success, or any errors otherwise.
    ///
    /// Retries when `io::Error::kind() == io::ErrorKind::Interrupted`.
    pub fn save_in_limited<P: AsRef<Path>>(&mut self, dir: P, limit: u64) -> io::Result<SavedFile> {
        let path = dir.as_ref().join(::random_alphanumeric(RANDOM_FILENAME_LEN));
        self.save_as_limited(path, limit)
    }

    /// Get the filename of this entry, if supplied.
    ///
    /// ##Warning
    /// You should treat this value as untrustworthy because it is an arbitrary string provided by
    /// the client. You should *not* blindly append it to a directory path and save the file there,
    /// as such behavior could easily be exploited by a malicious client.
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_ref().map(String::as_ref)
    }

    /// Get the MIME type (`Content-Type` value) of this file, if supplied by the client,
    /// or `"applicaton/octet-stream"` otherwise.
    pub fn content_type(&self) -> &Mime {
        &self.content_type
    }
}

impl<'a, B: Read> Read for MultipartFile<'a, B> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>{
        self.stream.read(buf)
    }
}

impl<'a, B: Read> BufRead for MultipartFile<'a, B> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.stream.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.stream.consume(amt)
    }
}

/// A file saved to the local filesystem from a multipart request.
#[derive(Debug)]
pub struct SavedFile {
    /// The complete path this file was saved at.
    pub path: PathBuf,

    /// The original filename of this file, if one was provided in the request.
    ///
    /// ##Warning
    /// You should treat this value as untrustworthy because it is an arbitrary string provided by
    /// the client. You should *not* blindly append it to a directory path and save the file there,
    /// as such behavior could easily be exploited by a malicious client.
    pub filename: Option<String>,

    /// The number of bytes written to the disk; may be truncated.
    pub size: u64,
}



fn read_content_type(cont_type: &str) -> Mime {
    cont_type.parse().ok().unwrap_or_else(::mime_guess::octet_stream)
}

fn split_once(s: &str, delim: char) -> Option<(&str, &str)> {
    s.find(delim).map(|idx| s.split_at(idx))
}

fn trim_quotes(s: &str) -> &str {
    s.trim_matches('"')
}

/// Get the string after `needle` in `haystack`, stopping before `end_val_delim`
fn get_str_after<'a>(needle: &str, end_val_delim: char, haystack: &'a str) -> Option<(&'a str, &'a str)> {
    let val_start_idx = try_opt!(haystack.find(needle)) + needle.len();
    let val_end_idx = haystack[val_start_idx..].find(end_val_delim)
        .map_or(haystack.len(), |end_idx| end_idx + val_start_idx);
    Some((&haystack[val_start_idx..val_end_idx], &haystack[val_end_idx..]))
}

fn io_str_utf8(buf: &[u8]) -> io::Result<&str> {
    str::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn find_header<'a, 'b>(headers: &'a [StrHeader<'b>], name: &str) -> Option<&'a StrHeader<'b>> {
    headers.iter().find(|header| header.name == name)
}

fn retry_on_interrupt<F, T>(mut do_fn: F) -> io::Result<T> where F: FnMut() -> io::Result<T> {
    loop {
        match do_fn() {
            Ok(val) => return Ok(val),
            Err(err) => if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            },
        }
    }
}

fn create_full_path(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        try!(fs::create_dir_all(parent));
    } else {
        // RFC: return an error instead?
        warn!("Attempting to save file in what looks like a root directory. File path: {:?}", path);
    }

    File::create(&path)
}