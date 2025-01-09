use local_encoding_ng::{Encoder, Encoding};
use std::ffi::OsString;
use std::io::{Error, ErrorKind, Read, Write};
use std::ptr;
use std::slice;

use thiserror::Error;

#[derive(Error, Clone, Debug)]
pub enum PostprocessError {
    #[error("unexpected end of line in literal")]
    LiteralEol,
    #[error("unexpected end of stream in literal")]
    LiteralEof,
    #[error("literal too long")]
    LiteralTooLong,
    #[error("unexpected end of escape sequence")]
    EscapeEof,
    #[error("can't find precompiled header marker: {0:?}")]
    MarkerNotFound(OsString),
    #[error("token too long")]
    TokenTooLong,
}

const BUF_SIZE: usize = 0x10000;

pub fn filter_preprocessed(
    reader: &mut impl Read,
    writer: &mut impl Write,
    marker: &Option<OsString>,
    keep_headers: bool,
) -> crate::Result<()> {
    let mut state = ScannerState {
        buf_data: [0; BUF_SIZE],
        ptr_copy: ptr::null(),
        ptr_read: ptr::null(),
        ptr_end: ptr::null(),

        reader,
        writer,

        keep_headers,
        marker: None,
        utf8: false,
        header_found: false,
        entry_file: None,
        done: false,
    };

    unsafe {
        state.ptr_copy = state.buf_data.as_ptr();
        state.ptr_read = state.buf_data.as_ptr();
        state.ptr_end = state.buf_data.as_ptr();

        state.parse_bom()?;
        state.marker = match marker.as_ref() {
            Some(v) => {
                let m = v.to_str().unwrap().to_string();
                if state.utf8 {
                    Some(Vec::from(m.as_bytes()))
                } else {
                    Some(Encoding::ANSI.to_bytes(&m.replace('\\', "/"))?)
                }
            }
            None => None,
        };
        loop {
            if state.ptr_read == state.ptr_end && !state.read()? {
                break;
            }
            state.parse_line()?;
            if state.done {
                return state.copy_to_end();
            }
        }
        Err(PostprocessError::MarkerNotFound(marker.clone().unwrap()).into())
    }
}

struct ScannerState<'a, R, W>
where
    R: Read,
    W: Write,
{
    buf_data: [u8; BUF_SIZE],
    ptr_copy: *const u8,
    ptr_read: *const u8,
    ptr_end: *const u8,

    reader: &'a mut R,
    writer: &'a mut W,

    keep_headers: bool,
    marker: Option<Vec<u8>>,

    utf8: bool,
    header_found: bool,
    entry_file: Option<Vec<u8>>,
    done: bool,
}

impl<R, W> ScannerState<'_, R, W>
where
    R: Read,
    W: Write,
{
    unsafe fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.flush()?;
        self.writer.write_all(data)?;
        Ok(())
    }

    #[inline(always)]
    unsafe fn peek(&mut self) -> Result<Option<u8>, Error> {
        if self.ptr_read == self.ptr_end && !self.read()? {
            return Ok(None);
        }
        Ok(Some(*self.ptr_read))
    }

    #[inline(always)]
    unsafe fn next(&mut self) {
        debug_assert!(self.ptr_read != self.ptr_end);
        self.ptr_read = self.ptr_read.offset(1);
    }

    unsafe fn read(&mut self) -> Result<bool, Error> {
        debug_assert!(self.ptr_read == self.ptr_end);
        self.flush()?;
        let base = self.buf_data.as_ptr();
        self.ptr_read = base;
        self.ptr_copy = base;
        self.ptr_end = base.add(self.reader.read(&mut self.buf_data)?);
        Ok(self.ptr_read != self.ptr_end)
    }

    unsafe fn copy_to_end(&mut self) -> crate::Result<()> {
        self.writer.write_all(slice::from_raw_parts(
            self.ptr_copy,
            delta(self.ptr_copy, self.ptr_end),
        ))?;
        self.ptr_copy = self.buf_data.as_ptr();
        self.ptr_end = self.buf_data.as_ptr();
        loop {
            match self.reader.read(&mut self.buf_data)? {
                0 => {
                    return Ok(());
                }
                size => {
                    self.writer.write_all(&self.buf_data[0..size])?;
                }
            }
        }
    }

    unsafe fn flush(&mut self) -> Result<(), Error> {
        if self.ptr_copy != self.ptr_read {
            if self.keep_headers {
                self.writer.write_all(slice::from_raw_parts(
                    self.ptr_copy,
                    delta(self.ptr_copy, self.ptr_read),
                ))?;
            }
            self.ptr_copy = self.ptr_read;
        }
        Ok(())
    }

    unsafe fn parse_bom(&mut self) -> Result<(), Error> {
        let bom: [u8; 3] = [0xEF, 0xBB, 0xBF];
        for bom_char in &bom {
            match self.peek()? {
                Some(c) if c == *bom_char => {
                    self.next();
                }
                Some(_) | None => {
                    return Ok(());
                }
            };
        }
        self.utf8 = true;
        Ok(())
    }

    unsafe fn parse_line(&mut self) -> Result<(), Error> {
        self.parse_empty()?;
        match self.peek()? {
            Some(b'#') => {
                self.next();
                self.parse_directive()
            }
            Some(_) => {
                self.next_line()?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    unsafe fn next_line(&mut self) -> Result<(), Error> {
        loop {
            let end = libc::memchr(
                self.ptr_read as *const libc::c_void,
                i32::from(b'\n'),
                delta(self.ptr_read, self.ptr_end),
            ) as *const u8;
            if !end.is_null() {
                self.ptr_read = end.offset(1);
                return Ok(());
            }
            self.ptr_read = self.ptr_end;
            if !self.read()? {
                return Ok(());
            }
        }
    }

    unsafe fn next_line_eol(&mut self) -> Result<&'static [u8], Error> {
        let mut last: u8 = 0;
        loop {
            let end = libc::memchr(
                self.ptr_read as *const libc::c_void,
                i32::from(b'\n'),
                delta(self.ptr_read, self.ptr_end),
            ) as *const u8;
            if !end.is_null() {
                if end != &self.buf_data[0] {
                    last = *end.offset(-1);
                }
                self.ptr_read = end.offset(1);
                if last == b'\r' {
                    return Ok(b"\r\n");
                }
                return Ok(b"\n");
            }

            if self.ptr_end == &self.buf_data[0] {
                last = 0;
            } else {
                last = *self.ptr_end.offset(-1);
            }
            self.ptr_read = self.ptr_end;
            if !self.read()? {
                return Ok(b"");
            }
        }
    }

    unsafe fn parse_directive(&mut self) -> Result<(), Error> {
        self.parse_spaces()?;
        let mut token = [0; 0x10];
        match self.parse_token(&mut token)? {
            b"line" => self.parse_directive_line(),
            b"pragma" => self.parse_directive_pragma(),
            _ => {
                self.next_line()?;
                Ok(())
            }
        }
    }

    unsafe fn parse_directive_line(&mut self) -> Result<(), Error> {
        let mut line_token = [0; 0x10];
        let mut file_token = [0; 0x400];
        let mut file_raw = [0; 0x400];
        self.parse_spaces()?;
        let line = self.parse_token(&mut line_token)?;
        self.parse_spaces()?;
        let (file, raw) = self.parse_path(&mut file_token, &mut file_raw)?;
        let eol = self.next_line_eol()?;
        self.entry_file = match self.entry_file.take() {
            Some(path) => {
                if self.header_found && (path == file) {
                    self.done = true;
                    let mut mark = Vec::with_capacity(0x400);
                    mark.write_all(b"#pragma hdrstop")?;
                    mark.write_all(eol)?;
                    mark.write_all(b"#line ")?;
                    mark.write_all(line)?;
                    mark.write_all(b" ")?;
                    mark.write_all(raw)?;
                    mark.write_all(eol)?;
                    self.write(&mark)?;
                }
                if let Some(ref path) = self.marker {
                    if is_subpath(file, path) {
                        self.header_found = true;
                    }
                }
                Some(path)
            }
            None => Some(Vec::from(file)),
        };
        Ok(())
    }

    unsafe fn parse_directive_pragma(&mut self) -> Result<(), Error> {
        self.parse_spaces()?;
        let mut token = [0; 0x20];
        match self.parse_token(&mut token)? {
            b"hdrstop" => {
                if !self.keep_headers {
                    self.write(b"#pragma hdrstop")?;
                }
                self.done = true;
            }
            _ => {
                self.next_line()?;
            }
        }
        Ok(())
    }

    unsafe fn parse_escape(&mut self) -> Result<u8, Error> {
        self.next();
        match self.peek()? {
            Some(c) => {
                self.next();
                match c {
                    b'n' => Ok(b'\n'),
                    b'r' => Ok(b'\r'),
                    b't' => Ok(b'\t'),
                    c => Ok(c),
                }
            }
            None => Err(Error::new(
                ErrorKind::InvalidInput,
                PostprocessError::EscapeEof,
            )),
        }
    }

    unsafe fn parse_spaces(&mut self) -> Result<(), Error> {
        loop {
            while self.ptr_read != self.ptr_end {
                match *self.ptr_read {
                    // non-nl-white-space ::= a blank, tab, or formfeed character
                    b' ' | b'\t' | b'\x0C' => {
                        self.next();
                    }
                    _ => {
                        return Ok(());
                    }
                }
            }
            if !self.read()? {
                return Ok(());
            }
        }
    }

    unsafe fn parse_empty(&mut self) -> Result<(), Error> {
        loop {
            while self.ptr_read != self.ptr_end {
                match *self.ptr_read {
                    // non-nl-white-space ::= a blank, tab, or formfeed character
                    b' ' | b'\t' | b'\x0C' | b'\n' | b'\r' => {
                        self.next();
                    }
                    _ => {
                        return Ok(());
                    }
                }
            }
            if !self.read()? {
                return Ok(());
            }
        }
    }

    unsafe fn parse_token<'b>(&mut self, token: &'b mut [u8]) -> Result<&'b [u8], Error> {
        let mut offset: usize = 0;
        loop {
            while self.ptr_read != self.ptr_end {
                let c: u8 = *self.ptr_read;
                match c {
                    // end-of-line ::= newline | carriage-return | carriage-return newline
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' => {
                        if offset >= token.len() {
                            return Err(Error::new(
                                ErrorKind::InvalidInput,
                                PostprocessError::TokenTooLong,
                            ));
                        }
                        token[offset] = c;
                        offset += 1;
                    }
                    _ => {
                        return Ok(&token[0..offset]);
                    }
                }
                self.next();
            }
            if !self.read()? {
                return Ok(token);
            }
        }
    }

    unsafe fn parse_path<'t, 'r>(
        &mut self,
        token: &'t mut [u8],
        raw: &'r mut [u8],
    ) -> Result<(&'t [u8], &'r [u8]), Error> {
        let quote = self.peek()?.unwrap();
        raw[0] = quote;
        self.next();
        let mut token_offset = 0;
        let mut raw_offset = 1;
        loop {
            while self.ptr_read != self.ptr_end {
                let c: u8 = *self.ptr_read;
                match c {
                    // end-of-line ::= newline | carriage-return | carriage-return newline
                    b'\n' | b'\r' => {
                        return Err(Error::new(
                            ErrorKind::InvalidInput,
                            PostprocessError::LiteralEol,
                        ));
                    }
                    b'\\' => {
                        raw[raw_offset] = b'\\';
                        raw[raw_offset + 1] = c;
                        raw_offset += 2;
                        token[token_offset] = match self.parse_escape()? {
                            b'\\' => b'/',
                            v => v,
                        };
                        token_offset += 1;
                    }
                    c => {
                        self.next();
                        raw[raw_offset] = c;
                        raw_offset += 1;
                        if c == quote {
                            return Ok((&token[..token_offset], &raw[..raw_offset]));
                        }
                        token[token_offset] = c;
                        token_offset += 1;
                    }
                }
                if (raw_offset >= raw.len() - 2) || (token_offset >= token.len() - 1) {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        PostprocessError::LiteralTooLong,
                    ));
                }
            }
            if !self.read()? {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    PostprocessError::LiteralEof,
                ));
            }
        }
    }
}

fn is_subpath(parent: &[u8], child: &[u8]) -> bool {
    if parent.len() < child.len() {
        return false;
    }
    if (parent.len() != child.len()) && (parent[parent.len() - child.len() - 1] != b'/') {
        return false;
    }
    child.eq_ignore_ascii_case(&parent[parent.len() - child.len()..])
}

unsafe fn delta(beg: *const u8, end: *const u8) -> usize {
    (end as usize) - (beg as usize)
}

#[cfg(test)]
mod test {
    use std::ffi::OsString;
    use std::io::{Cursor, Write};

    fn check_filter_pass(
        original: &str,
        expected: &str,
        marker: &Option<OsString>,
        keep_headers: bool,
        eol: &str,
    ) {
        let mut writer: Vec<u8> = Vec::new();
        let mut stream: Vec<u8> = Vec::new();
        stream
            .write_all(original.replace('\n', eol).as_bytes())
            .unwrap();
        match super::filter_preprocessed(
            &mut Cursor::new(stream),
            &mut writer,
            marker,
            keep_headers,
        ) {
            Ok(_) => assert_eq!(
                String::from_utf8_lossy(&writer),
                expected.replace('\n', eol)
            ),
            Err(e) => {
                panic!("{}", e);
            }
        }
    }

    fn check_filter(original: &str, expected: &str, marker: Option<OsString>, keep_headers: bool) {
        check_filter_pass(original, expected, &marker, keep_headers, "\n");
        check_filter_pass(original, expected, &marker, keep_headers, "\r\n");
    }

    #[test]
    fn test_filter_precompiled_keep() {
        check_filter(
            r#"#line 1 "sample.cpp"
#line 1 "e:/work/octobuild/test_cl/sample header.h"
# pragma once
void hello();
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            r#"#line 1 "sample.cpp"
#line 1 "e:/work/octobuild/test_cl/sample header.h"
# pragma once
void hello();
#line 2 "sample.cpp"
#pragma hdrstop
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            Some(OsString::from("sample header.h")),
            true,
        )
    }

    #[test]
    fn test_filter_precompiled_remove() {
        check_filter(
            r#"#line 1 "sample.cpp"
#line 1 "e:/work/octobuild/test_cl/sample header.h"
# pragma once
void hello1();
void hello2();
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            r#"#pragma hdrstop
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            Some(OsString::from("sample header.h")),
            false,
        );
    }

    #[test]
    fn test_filter_precompiled_case() {
        check_filter(
            r#"#line 1 "sample.cpp"
#line 1 "e:/work/octobuild/test_cl/StdAfx.h"
# pragma once
void hello1();
void hello2();
#line 2 "sample.cpp"

int main(int argc, char **argv) {
    return 0;
}
"#,
            r#"#pragma hdrstop
#line 2 "sample.cpp"

int main(int argc, char **argv) {
    return 0;
}
"#,
            Some(OsString::from("STDafx.h")),
            false,
        );
    }

    #[test]
    fn test_filter_precompiled_hdrstop() {
        check_filter(
            r#"#line 1 "sample.cpp"
 #line 1 "e:/work/octobuild/test_cl/sample header.h"
void hello();
# pragma  hdrstop
void data();
# pragma once
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            r#"#pragma hdrstop
void data();
# pragma once
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            None,
            false,
        );
    }

    #[test]
    fn test_filter_precompiled_hdrstop_keep() {
        check_filter(
            r#"#line 1 "sample.cpp"
 #line 1 "e:/work/octobuild/test_cl/sample header.h"
void hello();
# pragma  hdrstop
void data();
# pragma once
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            r#"#line 1 "sample.cpp"
 #line 1 "e:/work/octobuild/test_cl/sample header.h"
void hello();
# pragma  hdrstop
void data();
# pragma once
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            None,
            true,
        );
    }

    #[test]
    fn test_filter_precompiled_winpath() {
        check_filter(
            r#"#line 1 "sample.cpp"
#line 1 "e:\\work\\octobuild\\test_cl\\sample header.h"
# pragma once
void hello();
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            r#"#line 1 "sample.cpp"
#line 1 "e:\\work\\octobuild\\test_cl\\sample header.h"
# pragma once
void hello();
#line 2 "sample.cpp"
#pragma hdrstop
#line 2 "sample.cpp"

int main(int argc, char **argv) {
	return 0;
}
"#,
            Some(OsString::from(
                "e:\\work\\octobuild\\test_cl\\sample header.h",
            )),
            true,
        );
    }
}
