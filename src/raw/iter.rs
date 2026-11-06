use std::convert::TryInto;

use crate::{
    Bson,
    DateTime,
    Decimal128,
    RawBson,
    Timestamp,
    oid::ObjectId,
    raw::{
        CStr,
        Error,
        MIN_BSON_DOCUMENT_SIZE,
        MIN_CODE_WITH_SCOPE_SIZE,
        Result,
        bool_from_slice,
        cstring_bytes,
        f64_from_slice,
        read_cstring_with_max_len,
        read_lenencode_bytes,
        try_to_str,
        value::RawValue,
    },
    spec::ElementType,
};

use super::{
    RawArray,
    RawBinaryRef,
    RawBsonRef,
    RawDocument,
    checked_add,
    i32_from_slice,
    i64_from_slice,
    read_len,
};

/// An iterator over the key-value pairs in a document. Construct by calling [`RawDocument::iter`]
/// or [`RawDocumentBuf::iter`](crate::RawDocumentBuf::iter).
pub struct Iter<'a> {
    inner: RawIter<'a>,
}

impl<'a> Iter<'a> {
    pub(crate) fn new(doc: &'a RawDocument) -> Self {
        Iter {
            inner: RawIter::new(doc),
        }
    }

    /// The maximum number of bytes the iterator should parse when searching for the null-terminator
    /// for a cstring.
    #[cfg(feature = "sfp-internal")]
    #[doc(hidden)]
    pub fn max_cstr_parse_len(mut self, len: impl Into<Option<usize>>) -> Self {
        self.inner = self.inner.max_cstr_parse_len(len);
        self
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = Result<(&'a CStr, RawBsonRef<'a>)>;

    fn next(&mut self) -> Option<Result<(&'a CStr, RawBsonRef<'a>)>> {
        match self.inner.next() {
            Some(Ok(elem)) => match elem.value() {
                Err(e) => Some(Err(e)),
                Ok(value) => Some(Ok((elem.key, value))),
            },
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }
}

/// An iterator over the elements in a document. Construct by calling [`RawDocument::iter_elements`]
/// or [`RawDocumentBuf::iter_elements`](crate::RawDocumentBuf::iter_elements).
pub struct RawIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    max_cstr_parse_len: Option<usize>,
    /// Whether the underlying doc is assumed to be valid or if an error has been encountered.
    /// After an error, all subsequent iterations will return None.
    valid: bool,

    /// When true, the iterator treats a null byte as the document terminator only at the last
    /// position of `bytes`; a null encountered earlier is an error. When false (used by
    /// `new_unchecked` for flat parsers that span nested documents), any null byte ends
    /// iteration.
    strict: bool,
}

impl<'a> RawIter<'a> {
    pub(crate) fn new(doc: &'a RawDocument) -> Self {
        Self {
            bytes: doc.as_bytes(),
            offset: 4,
            max_cstr_parse_len: None,
            valid: true,
            strict: true,
        }
    }

    #[cfg(feature = "facet-unstable")]
    pub(crate) fn new_unchecked(bytes: &'a [u8], offset: usize) -> Self {
        Self {
            bytes,
            offset,
            max_cstr_parse_len: None,
            valid: true,
            strict: false,
        }
    }

    /// The maximum number of bytes the iterator should parse when searching for the null-terminator
    /// for a cstring.
    #[cfg(feature = "sfp-internal")]
    #[doc(hidden)]
    pub fn max_cstr_parse_len(mut self, len: impl Into<Option<usize>>) -> Self {
        self.max_cstr_parse_len = len.into();
        self
    }

    fn verify_enough_bytes(&self, start: usize, num_bytes: usize) -> Result<()> {
        let end = checked_add(start, num_bytes)?;
        if self.bytes.get(start..end).is_none() {
            return Err(Error::malformed_bytes(format!(
                "length exceeds remaining length of buffer: {} vs {}",
                num_bytes,
                self.bytes.len() - start
            )));
        }
        Ok(())
    }

    pub(crate) fn next_document_len(&self, starting_at: usize) -> Result<usize> {
        self.verify_enough_bytes(starting_at, MIN_BSON_DOCUMENT_SIZE as usize)?;
        let size = i32_from_slice(&self.bytes[starting_at..])? as usize;

        if size < MIN_BSON_DOCUMENT_SIZE as usize {
            return Err(Error::malformed_bytes(format!(
                "document too small: {} bytes",
                size
            )));
        }

        self.verify_enough_bytes(starting_at, size)?;

        if self.bytes[starting_at + size - 1] != 0 {
            return Err(Error::malformed_bytes("not null terminated"));
        }
        Ok(size)
    }
}

/// A view into a value contained in a [`RawDocument`] or [`RawDocumentBuf`](crate::RawDocumentBuf).
/// The underlying bytes of the element are not parsed or validated; call [`RawElement::value`] or
/// one of the `TryFrom` implementations to convert the element into a BSON value.
#[derive(Clone)]
pub struct RawElement<'a> {
    key: &'a CStr,
    value: RawValue<'a>,
}

impl<'a> TryFrom<RawElement<'a>> for RawBsonRef<'a> {
    type Error = Error;

    fn try_from(element: RawElement<'a>) -> Result<Self> {
        element.value()
    }
}

impl TryFrom<RawElement<'_>> for RawBson {
    type Error = Error;

    fn try_from(element: RawElement<'_>) -> Result<Self> {
        Ok(element.value()?.into())
    }
}

impl TryFrom<RawElement<'_>> for Bson {
    type Error = Error;

    fn try_from(element: RawElement<'_>) -> Result<Self> {
        element.value()?.try_into()
    }
}

impl<'a> RawElement<'a> {
    #[cfg(feature = "serde")]
    pub(crate) fn toplevel(bytes: &'a [u8]) -> Self {
        use crate::raw::cstr;

        Self {
            key: cstr!("TOPLEVEL"),
            value: RawValue::new(ElementType::EmbeddedDocument, bytes),
        }
    }

    /// The size of the element.
    pub fn size(&self) -> usize {
        self.value.bytes().len()
    }

    /// The document key the element corresponds to.
    pub fn key(&self) -> &'a CStr {
        self.key
    }

    /// The type of the element.
    pub fn element_type(&self) -> ElementType {
        self.value.kind()
    }

    /// Parses this element into a [`RawBsonRef`] and returns an error if the underlying bytes are
    /// invalid.
    pub fn value(&self) -> Result<RawBsonRef<'a>> {
        self.value
            .parse()
            .map_err(|e| e.with_key(self.key.as_str()))
    }

    #[cfg(any(feature = "serde", feature = "facet-unstable"))]
    pub(crate) fn value_raw(&self) -> &RawValue<'a> {
        &self.value
    }

    /// Parses this element into [`RawBson`], replacing any invalid UTF-8 strings with the Unicode
    /// replacement character. Returns an error if the underlying bytes are invalid.
    pub fn value_utf8_lossy(&self) -> Result<RawBson> {
        match self.value_utf8_lossy_inner()? {
            Some(v) => Ok(v.into()),
            None => Ok(self.value()?.into()),
        }
    }

    pub(crate) fn value_utf8_lossy_inner(&self) -> Result<Option<Utf8LossyBson<'a>>> {
        self.value
            .parse_utf8_lossy()
            .map_err(|e| e.with_key(self.key.as_str()))
    }
}

impl RawIter<'_> {
    fn get_next_length_at(&self, start_at: usize) -> Result<usize> {
        let len = i32_from_slice(&self.bytes[start_at..])?;
        if len < 0 {
            Err(Error::malformed_bytes("lengths can't be negative"))
        } else {
            Ok(len as usize)
        }
    }

    fn get_next_kvp(&mut self, offset: usize) -> Result<(ElementType, usize)> {
        let element_type = match ElementType::from(self.bytes[self.offset]) {
            Some(et) => et,
            None => {
                return Err(Error::malformed_bytes(format!(
                    "invalid tag: {}",
                    self.bytes[self.offset]
                )));
            }
        };

        let element_size = match element_type {
            ElementType::Boolean => 1,
            ElementType::Int32 => 4,
            ElementType::Int64 => 8,
            ElementType::Double => 8,
            ElementType::DateTime => 8,
            ElementType::Timestamp => 8,
            ElementType::ObjectId => 12,
            ElementType::Decimal128 => 16,
            ElementType::Null => 0,
            ElementType::Undefined => 0,
            ElementType::MinKey => 0,
            ElementType::MaxKey => 0,
            ElementType::String => read_len(&self.bytes[offset..])?,
            ElementType::EmbeddedDocument => self.next_document_len(offset)?,
            ElementType::Array => self.next_document_len(offset)?,
            ElementType::Binary => self.get_next_length_at(offset)? + 4 + 1,
            ElementType::RegularExpression => {
                let pattern =
                    read_cstring_with_max_len(&self.bytes[offset..], self.max_cstr_parse_len)?;
                let options = read_cstring_with_max_len(
                    &self.bytes[offset + pattern.len() + 1..],
                    self.max_cstr_parse_len,
                )?;
                pattern.len() + 1 + options.len() + 1
            }
            ElementType::DbPointer => read_len(&self.bytes[offset..])? + 12,
            ElementType::Symbol => read_len(&self.bytes[offset..])?,
            ElementType::JavaScriptCode => read_len(&self.bytes[offset..])?,
            ElementType::JavaScriptCodeWithScope => self.get_next_length_at(offset)?,
        };

        self.verify_enough_bytes(offset, element_size)?;
        self.offset = offset + element_size;

        Ok((element_type, element_size))
    }
}

impl<'a> Iterator for RawIter<'a> {
    type Item = Result<RawElement<'a>>;

    fn next(&mut self) -> Option<Result<RawElement<'a>>> {
        if !self.valid {
            return None;
        } else if self.offset >= self.bytes.len() {
            self.valid = false;
            return Some(Err(Error::malformed_bytes("iteration overflowed document")));
        } else if self.strict {
            if self.offset == self.bytes.len() - 1 {
                return if self.bytes[self.offset] == 0 {
                    None
                } else {
                    self.valid = false;
                    Some(Err(Error::malformed_bytes("document not null terminated")))
                };
            }
        } else if self.bytes[self.offset] == 0 {
            // end of document marker
            return None;
        }

        let key = match read_cstring_with_max_len(
            &self.bytes[self.offset + 1..],
            self.max_cstr_parse_len,
        ) {
            Ok(k) => k,
            Err(e) => {
                self.valid = false;
                return Some(Err(e));
            }
        };
        let offset = self.offset + 1 + key.len() + 1; // type specifier + key + \0

        Some(match self.get_next_kvp(offset) {
            Ok((kind, size)) => Ok(RawElement {
                key,
                value: RawValue::new_at(kind, &self.bytes[offset..offset + size], offset),
            }),
            Err(error) => {
                self.valid = false;
                Err(error.with_key(key.as_str()))
            }
        })
    }
}

pub(crate) enum Utf8LossyBson<'a> {
    String(String),
    JavaScriptCode(String),
    JavaScriptCodeWithScope(Utf8LossyJavaScriptCodeWithScope<'a>),
    Symbol(String),
    DbPointer(crate::DbPointer),
    RegularExpression(crate::Regex),
}

pub(crate) struct Utf8LossyJavaScriptCodeWithScope<'a> {
    pub(crate) code: String,
    pub(crate) scope: &'a RawDocument,
}

impl<'a> From<Utf8LossyBson<'a>> for RawBson {
    fn from(value: Utf8LossyBson<'a>) -> Self {
        match value {
            Utf8LossyBson::String(s) => RawBson::String(s),
            Utf8LossyBson::JavaScriptCode(s) => RawBson::JavaScriptCode(s),
            Utf8LossyBson::JavaScriptCodeWithScope(Utf8LossyJavaScriptCodeWithScope {
                code,
                scope,
            }) => RawBson::JavaScriptCodeWithScope(super::RawJavaScriptCodeWithScope {
                code,
                scope: scope.to_owned(),
            }),
            Utf8LossyBson::Symbol(s) => RawBson::Symbol(s),
            Utf8LossyBson::DbPointer(p) => RawBson::DbPointer(p),
            Utf8LossyBson::RegularExpression(r) => RawBson::RegularExpression(r),
        }
    }
}

/// An iterator over views of the elements in a document. Construct by calling
/// [`RawDocument::iter_element_views`],
/// [`RawDocumentBuf::iter_element_views`](crate::RawDocumentBuf::iter_element_views), or
/// [`RawArray::iter_element_views`](crate::RawArray::iter_element_views).
///
/// This iterator only validates the structure needed to locate each element (element type tags,
/// lengths, null terminators, and bounds). It does not perform any UTF-8 validation; keys and
/// string-like values are exposed as raw bytes so that the caller can decide how to decode them.
/// An error is returned and iteration ends if an element cannot be located.
///
/// Iteration ends cleanly (i.e. [`Iterator::next`] returns `None`) only when the terminating null
/// byte is reached as the final byte of the document. Any byte of the document's body that is not
/// consumed by an element (including a premature null type tag) produces an error, so a completed
/// iteration accounts for the entire document.
pub struct RawElementViewIter<'a> {
    bytes: &'a [u8],
    offset: usize,
    /// Whether the underlying doc is assumed to be valid or if an error has been encountered.
    /// After an error, all subsequent iterations will return None.
    valid: bool,
}

impl<'a> RawElementViewIter<'a> {
    pub(crate) fn new(doc: &'a RawDocument) -> Self {
        Self {
            bytes: doc.as_bytes(),
            offset: 4,
            valid: true,
        }
    }

    fn verify_enough_bytes(&self, start: usize, num_bytes: usize) -> Result<()> {
        let end = checked_add(start, num_bytes)?;
        if self.bytes.get(start..end).is_none() {
            return Err(Error::malformed_bytes(format!(
                "length exceeds remaining length of buffer: {} vs {}",
                num_bytes,
                self.bytes.len() - start
            )));
        }
        Ok(())
    }

    fn get_next_length_at(&self, start_at: usize) -> Result<usize> {
        let len = i32_from_slice(&self.bytes[start_at..])?;
        if len < 0 {
            Err(Error::malformed_bytes("lengths can't be negative"))
        } else {
            Ok(len as usize)
        }
    }

    fn next_document_len(&self, starting_at: usize) -> Result<usize> {
        self.verify_enough_bytes(starting_at, MIN_BSON_DOCUMENT_SIZE as usize)?;
        let size = i32_from_slice(&self.bytes[starting_at..])? as usize;

        if size < MIN_BSON_DOCUMENT_SIZE as usize {
            return Err(Error::malformed_bytes(format!(
                "document too small: {} bytes",
                size
            )));
        }

        self.verify_enough_bytes(starting_at, size)?;

        if self.bytes[starting_at + size - 1] != 0 {
            return Err(Error::malformed_bytes("not null terminated"));
        }
        Ok(size)
    }

    /// Calculates the size of the value of an element of `element_type` whose value begins at
    /// `offset`, performing only the structural validation needed to locate the element's extent
    /// (lengths, null terminators, and bounds). No UTF-8 validation is performed.
    fn value_size_at(&self, offset: usize, element_type: ElementType) -> Result<usize> {
        let size = match element_type {
            ElementType::Boolean => 1,
            ElementType::Int32 => 4,
            ElementType::Int64 => 8,
            ElementType::Double => 8,
            ElementType::DateTime => 8,
            ElementType::Timestamp => 8,
            ElementType::ObjectId => 12,
            ElementType::Decimal128 => 16,
            ElementType::Null => 0,
            ElementType::Undefined => 0,
            ElementType::MinKey => 0,
            ElementType::MaxKey => 0,
            ElementType::String | ElementType::Symbol | ElementType::JavaScriptCode => {
                read_len(&self.bytes[offset..])?
            }
            ElementType::EmbeddedDocument | ElementType::Array => self.next_document_len(offset)?,
            ElementType::Binary => self.get_next_length_at(offset)? + 4 + 1,
            ElementType::RegularExpression => {
                let pattern = cstring_bytes(&self.bytes[offset..])?;
                let options = cstring_bytes(&self.bytes[offset + pattern.len() + 1..])?;
                pattern.len() + 1 + options.len() + 1
            }
            ElementType::DbPointer => read_len(&self.bytes[offset..])? + 12,
            ElementType::JavaScriptCodeWithScope => self.get_next_length_at(offset)?,
        };

        self.verify_enough_bytes(offset, size)?;
        Ok(size)
    }
}

impl<'a> Iterator for RawElementViewIter<'a> {
    type Item = Result<RawElementView<'a>>;

    fn next(&mut self) -> Option<Result<RawElementView<'a>>> {
        if !self.valid {
            return None;
        } else if self.offset >= self.bytes.len() {
            self.valid = false;
            return Some(Err(Error::malformed_bytes("iteration overflowed document")));
        } else if self.offset == self.bytes.len() - 1 {
            return if self.bytes[self.offset] == 0 {
                None
            } else {
                self.valid = false;
                Some(Err(Error::malformed_bytes("document not null terminated")))
            };
        }

        let element_type = match ElementType::from(self.bytes[self.offset]) {
            Some(et) => et,
            None => {
                self.valid = false;
                return Some(Err(Error::malformed_bytes(format!(
                    "invalid tag: {} (at byte offset {})",
                    self.bytes[self.offset], self.offset
                ))));
            }
        };

        // Locate the key's null terminator without validating it as UTF-8.
        let key_start = self.offset + 1;
        let key_len = match self.bytes[key_start..].iter().position(|&b| b == 0) {
            Some(len) => len,
            None => {
                self.valid = false;
                return Some(Err(Error::malformed_bytes(format!(
                    "expected null terminator for key (at byte offset {})",
                    key_start
                ))));
            }
        };
        let key = &self.bytes[key_start..key_start + key_len];
        let value_offset = key_start + key_len + 1;

        Some(match self.value_size_at(value_offset, element_type) {
            Ok(size) => {
                self.offset = value_offset + size;
                Ok(RawElementView {
                    element_type,
                    key,
                    value: &self.bytes[value_offset..value_offset + size],
                    value_offset,
                })
            }
            Err(error) => {
                self.valid = false;
                Err(error)
            }
        })
    }
}

/// A view of the bytes of an element contained in a [`RawDocument`] or
/// [`RawDocumentBuf`](crate::RawDocumentBuf), yielded by [`RawElementViewIter`]. Neither the key
/// nor the value has been validated as UTF-8.
///
/// Call [`RawElementView::value`] to parse the value (validating all strings as UTF-8), call
/// [`RawElementView::value_view`] to parse the value's shape while leaving string-like payloads as
/// raw bytes, or convert the view into a [`RawElement`] via its `TryFrom` implementation to
/// validate the key.
///
/// ```
/// # use bson::raw::{RawBsonRefView, RawDocument};
/// // a document whose string value contains invalid UTF-8
/// let doc = RawDocument::from_bytes(b"\x12\0\0\0\x02a\0\x06\0\0\0h\xffllo\0\0")?;
///
/// let mut decoded = Vec::new();
/// for element in doc.iter_element_views() {
///     let element = element?;
///     match element.value_view()? {
///         RawBsonRefView::String(payload) => {
///             // the caller applies its own decoding policy; the framing has been parsed
///             decoded.push((element.key_bytes().to_vec(), payload.to_vec()));
///         }
///         _ => unreachable!(),
///     }
/// }
/// assert_eq!(decoded, [(b"a".to_vec(), b"h\xffllo".to_vec())]);
/// # Ok::<(), bson::error::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct RawElementView<'a> {
    element_type: ElementType,
    key: &'a [u8],
    value: &'a [u8],
    value_offset: usize,
}

impl<'a> RawElementView<'a> {
    /// The type of the element.
    pub fn element_type(&self) -> ElementType {
        self.element_type
    }

    /// The bytes of the document key the element corresponds to, excluding the null terminator.
    /// These bytes are not guaranteed to be valid UTF-8.
    pub fn key_bytes(&self) -> &'a [u8] {
        self.key
    }

    /// The bytes of the element's value, as encoded in the document. This includes any
    /// type-specific framing; for example, the value bytes of a string include its length prefix
    /// and null terminator. These bytes are not guaranteed to contain valid UTF-8.
    pub fn value_bytes(&self) -> &'a [u8] {
        self.value
    }

    /// Parses this element's value into a [`RawBsonRef`] and returns an error if the underlying
    /// bytes are invalid, including if any strings in the value are not valid UTF-8. The key is
    /// not validated.
    pub fn value(&self) -> Result<RawBsonRef<'a>> {
        RawValue::new_at(self.element_type, self.value, self.value_offset).parse()
    }

    /// Parses the shape of this element's value into a [`RawBsonRefView`], in which the payloads
    /// of string-like types ([`ElementType::String`], [`ElementType::Symbol`],
    /// [`ElementType::JavaScriptCode`], [`ElementType::JavaScriptCodeWithScope`],
    /// [`ElementType::DbPointer`], and [`ElementType::RegularExpression`]) are exposed as raw
    /// bytes, leaving the caller to apply its own decoding policy. All type-specific framing
    /// (length prefixes, null terminators, and nested extents) is parsed, and an error is returned
    /// if the underlying bytes are structurally invalid. The key is not validated, and no part of
    /// the value is validated as UTF-8.
    pub fn value_view(&self) -> Result<RawBsonRefView<'a>> {
        Ok(match self.element_type {
            ElementType::Null => RawBsonRefView::Null,
            ElementType::Undefined => RawBsonRefView::Undefined,
            ElementType::MinKey => RawBsonRefView::MinKey,
            ElementType::MaxKey => RawBsonRefView::MaxKey,
            ElementType::ObjectId => RawBsonRefView::ObjectId(ObjectId::parse(self.value)?),
            ElementType::Int32 => RawBsonRefView::Int32(i32_from_slice(self.value)?),
            ElementType::Int64 => RawBsonRefView::Int64(i64_from_slice(self.value)?),
            ElementType::Double => RawBsonRefView::Double(f64_from_slice(self.value)?),
            ElementType::Boolean => RawBsonRefView::Boolean(
                bool_from_slice(self.value).map_err(Error::malformed_bytes)?,
            ),
            ElementType::DateTime => RawBsonRefView::DateTime(DateTime::parse(self.value)?),
            ElementType::Decimal128 => RawBsonRefView::Decimal128(Decimal128::parse(self.value)?),
            ElementType::Timestamp => RawBsonRefView::Timestamp(Timestamp::parse(self.value)?),
            ElementType::Binary => RawBsonRefView::Binary(RawBinaryRef::parse(self.value)?),
            ElementType::String => RawBsonRefView::String(read_lenencode_bytes(self.value)?),
            ElementType::Symbol => RawBsonRefView::Symbol(read_lenencode_bytes(self.value)?),
            ElementType::JavaScriptCode => {
                RawBsonRefView::JavaScriptCode(read_lenencode_bytes(self.value)?)
            }
            ElementType::EmbeddedDocument => {
                RawBsonRefView::Document(RawDocument::from_bytes(self.value)?)
            }
            ElementType::Array => {
                RawBsonRefView::Array(RawArray::from_doc(RawDocument::from_bytes(self.value)?))
            }
            ElementType::RegularExpression => {
                let pattern = cstring_bytes(self.value)?;
                let options = cstring_bytes(&self.value[pattern.len() + 1..])?;
                RawBsonRefView::RegularExpression { pattern, options }
            }
            ElementType::DbPointer => {
                let namespace = read_lenencode_bytes(self.value)?;
                if self.value.len() < 12 {
                    return Err(Error::malformed_bytes("dbpointer too short"));
                }
                let id = ObjectId::from_bytes(
                    self.value[self.value.len() - 12..]
                        .try_into()
                        .map_err(Error::malformed_bytes)?,
                );
                RawBsonRefView::DbPointer { namespace, id }
            }
            ElementType::JavaScriptCodeWithScope => {
                if self.value.len() < MIN_CODE_WITH_SCOPE_SIZE as usize {
                    return Err(Error::malformed_bytes("code with scope length too small"));
                }

                let code = read_lenencode_bytes(&self.value[4..])?;
                // the scope follows the code's raw bytes; using the encoded length here would
                // shift the offset whenever the code contains multi-byte sequences
                let scope_start = 4 + 4 + code.len() + 1;
                if scope_start >= self.value.len() {
                    return Err(Error::malformed_bytes("code with scope length overrun"));
                }
                let scope = RawDocument::from_bytes(&self.value[scope_start..])?;

                RawBsonRefView::JavaScriptCodeWithScope { code, scope }
            }
        })
    }
}

impl<'a> TryFrom<RawElementView<'a>> for RawElement<'a> {
    type Error = Error;

    /// Validates the view's key and converts it into a [`RawElement`]. Returns an error if the key
    /// is not valid UTF-8.
    fn try_from(view: RawElementView<'a>) -> Result<Self> {
        let key: &CStr = try_to_str(view.key)?.try_into()?;
        Ok(Self {
            key,
            value: RawValue::new_at(view.element_type, view.value, view.value_offset),
        })
    }
}

/// A parsed view of a BSON value in which the payloads of string-like types are exposed as raw
/// bytes, leaving the caller to apply its own decoding policy. This is the byte-preserving analog
/// of [`RawBsonRef`]: all other variants are identical, and all type-specific framing has been
/// parsed and validated. Construct by calling [`RawElementView::value_view`].
///
/// Bytes are borrowed from the source document and are not guaranteed to be valid UTF-8.
#[derive(Clone, PartialEq)]
pub enum RawBsonRefView<'a> {
    /// 64-bit binary floating point
    Double(f64),
    /// UTF-8 string, as raw bytes excluding the length prefix and null terminator
    String(&'a [u8]),
    /// Array
    Array(&'a RawArray),
    /// Embedded document
    Document(&'a RawDocument),
    /// Boolean value
    Boolean(bool),
    /// Null value
    Null,
    /// Regular expression, with the pattern and options as raw bytes excluding their null
    /// terminators
    RegularExpression {
        /// The pattern bytes
        pattern: &'a [u8],
        /// The options bytes
        options: &'a [u8],
    },
    /// JavaScript code, as raw bytes excluding the length prefix and null terminator
    JavaScriptCode(&'a [u8]),
    /// JavaScript code w/ scope, with the code as raw bytes excluding the length prefix and null
    /// terminator, and the scope document validated for structural validity only
    JavaScriptCodeWithScope {
        /// The code bytes
        code: &'a [u8],
        /// The scope document
        scope: &'a RawDocument,
    },
    /// 32-bit signed integer
    Int32(i32),
    /// 64-bit signed integer
    Int64(i64),
    /// Timestamp
    Timestamp(Timestamp),
    /// Binary data
    Binary(RawBinaryRef<'a>),
    /// [ObjectId](http://dochub.mongodb.org/core/objectids)
    ObjectId(ObjectId),
    /// UTC datetime
    DateTime(DateTime),
    /// [128-bit decimal](https://github.com/mongodb/specifications/blob/master/source/bson-decimal128/decimal128.md)
    Decimal128(Decimal128),
    /// [DBPointer](https://bsonspec.org/spec.html), with the namespace as raw bytes excluding the
    /// length prefix and null terminator
    DbPointer {
        /// The namespace bytes
        namespace: &'a [u8],
        /// The referenced id
        id: ObjectId,
    },
    /// Deprecated UTF-8 string type, as raw bytes excluding the length prefix and null terminator
    Symbol(&'a [u8]),
    /// Deprecated value
    Undefined,
    /// Max key
    MaxKey,
    /// Min key
    MinKey,
}

impl std::fmt::Debug for RawBsonRefView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // string-like payloads are rendered lossily for diagnostic purposes
        fn bytes(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
            String::from_utf8_lossy(bytes)
        }

        match self {
            Self::Double(v) => f.debug_tuple("Double").field(v).finish(),
            Self::String(b) => f.debug_tuple("String").field(&bytes(b)).finish(),
            Self::Array(a) => f.debug_tuple("Array").field(a).finish(),
            Self::Document(d) => f.debug_tuple("Document").field(d).finish(),
            Self::Boolean(v) => f.debug_tuple("Boolean").field(v).finish(),
            Self::Null => f.write_str("Null"),
            Self::RegularExpression { pattern, options } => f
                .debug_struct("RegularExpression")
                .field("pattern", &bytes(pattern))
                .field("options", &bytes(options))
                .finish(),
            Self::JavaScriptCode(c) => f.debug_tuple("JavaScriptCode").field(&bytes(c)).finish(),
            Self::JavaScriptCodeWithScope { code, scope } => f
                .debug_struct("JavaScriptCodeWithScope")
                .field("code", &bytes(code))
                .field("scope", scope)
                .finish(),
            Self::Int32(v) => f.debug_tuple("Int32").field(v).finish(),
            Self::Int64(v) => f.debug_tuple("Int64").field(v).finish(),
            Self::Timestamp(v) => f.debug_tuple("Timestamp").field(v).finish(),
            Self::Binary(v) => f.debug_tuple("Binary").field(v).finish(),
            Self::ObjectId(v) => f.debug_tuple("ObjectId").field(v).finish(),
            Self::DateTime(v) => f.debug_tuple("DateTime").field(v).finish(),
            Self::Decimal128(v) => f.debug_tuple("Decimal128").field(v).finish(),
            Self::DbPointer { namespace, id } => f
                .debug_struct("DbPointer")
                .field("namespace", &bytes(namespace))
                .field("id", id)
                .finish(),
            Self::Symbol(b) => f.debug_tuple("Symbol").field(&bytes(b)).finish(),
            Self::Undefined => f.write_str("Undefined"),
            Self::MaxKey => f.write_str("MaxKey"),
            Self::MinKey => f.write_str("MinKey"),
        }
    }
}
