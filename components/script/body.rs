/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::dom::bindings::codegen::Bindings::FormDataBinding::FormDataMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::reflector::DomObject;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::USVString;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::blob::{Blob, BlobImpl};
use crate::dom::formdata::FormData;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use js::jsapi::Heap;
use js::jsapi::JSContext;
use js::jsapi::JSObject;
use js::jsapi::JS_ClearPendingException;
use js::jsapi::Value as JSValue;
use js::jsval::JSVal;
use js::jsval::UndefinedValue;
use js::rust::wrappers::JS_GetPendingException;
use js::rust::wrappers::JS_ParseJSON;
use js::typedarray::{ArrayBuffer, CreateWith};
use mime::{self, Mime};
use std::cell::Ref;
use std::ptr;
use std::rc::Rc;
use std::str;
use url::form_urlencoded;

#[derive(Clone, Copy, JSTraceable, MallocSizeOf)]
pub enum BodyType {
    Blob,
    FormData,
    Json,
    Text,
    ArrayBuffer,
}

pub enum FetchedData {
    Text(String),
    Json(RootedTraceableBox<Heap<JSValue>>),
    BlobData(DomRoot<Blob>),
    FormData(DomRoot<FormData>),
    ArrayBuffer(RootedTraceableBox<Heap<*mut JSObject>>),
    JSException(RootedTraceableBox<Heap<JSVal>>),
}

// https://fetch.spec.whatwg.org/#concept-body-consume-body
#[allow(unrooted_must_root)]
pub fn consume_body<T: BodyOperations + DomObject>(object: &T, body_type: BodyType) -> Rc<Promise> {
    let promise = Promise::new(&object.global());

    // Step 1
    if object.get_body_used() || object.is_locked() {
        promise.reject_error(Error::Type(
            "The response's stream is disturbed or locked".to_string(),
        ));
        return promise;
    }

    object.set_body_promise(&promise, body_type);

    // Steps 2-4
    // TODO: Body does not yet have a stream.

    consume_body_with_promise(object, body_type, &promise);

    promise
}

// https://fetch.spec.whatwg.org/#concept-body-consume-body
#[allow(unrooted_must_root)]
pub fn consume_body_with_promise<T: BodyOperations + DomObject>(
    object: &T,
    body_type: BodyType,
    promise: &Promise,
) {
    // Step 5
    let body = match object.take_body() {
        Some(body) => body,
        None => return,
    };

    let pkg_data_results =
        run_package_data_algorithm(object, body, body_type, object.get_mime_type());

    match pkg_data_results {
        Ok(results) => {
            match results {
                FetchedData::Text(s) => promise.resolve_native(&USVString(s)),
                FetchedData::Json(j) => promise.resolve_native(&j),
                FetchedData::BlobData(b) => promise.resolve_native(&b),
                FetchedData::FormData(f) => promise.resolve_native(&f),
                FetchedData::ArrayBuffer(a) => promise.resolve_native(&a),
                FetchedData::JSException(e) => promise.reject_native(&e.handle()),
            };
        },
        Err(err) => promise.reject_error(err),
    }
}

// https://fetch.spec.whatwg.org/#concept-body-package-data
#[allow(unsafe_code)]
fn run_package_data_algorithm<T: BodyOperations + DomObject>(
    object: &T,
    bytes: Vec<u8>,
    body_type: BodyType,
    mime_type: Ref<Vec<u8>>,
) -> Fallible<FetchedData> {
    let global = object.global();
    let cx = global.get_cx();
    let mime = &*mime_type;
    match body_type {
        BodyType::Text => run_text_data_algorithm(bytes),
        BodyType::Json => run_json_data_algorithm(cx, bytes),
        BodyType::Blob => run_blob_data_algorithm(&global, bytes, mime),
        BodyType::FormData => run_form_data_algorithm(&global, bytes, mime),
        BodyType::ArrayBuffer => unsafe { run_array_buffer_data_algorithm(cx, bytes) },
    }
}

fn run_text_data_algorithm(bytes: Vec<u8>) -> Fallible<FetchedData> {
    Ok(FetchedData::Text(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

#[allow(unsafe_code)]
fn run_json_data_algorithm(cx: *mut JSContext, bytes: Vec<u8>) -> Fallible<FetchedData> {
    let json_text = String::from_utf8_lossy(&bytes);
    let json_text: Vec<u16> = json_text.encode_utf16().collect();
    rooted!(in(cx) let mut rval = UndefinedValue());
    unsafe {
        if !JS_ParseJSON(
            cx,
            json_text.as_ptr(),
            json_text.len() as u32,
            rval.handle_mut(),
        ) {
            rooted!(in(cx) let mut exception = UndefinedValue());
            assert!(JS_GetPendingException(cx, exception.handle_mut()));
            JS_ClearPendingException(cx);
            return Ok(FetchedData::JSException(RootedTraceableBox::from_box(
                Heap::boxed(exception.get()),
            )));
        }
        let rooted_heap = RootedTraceableBox::from_box(Heap::boxed(rval.get()));
        Ok(FetchedData::Json(rooted_heap))
    }
}

fn run_blob_data_algorithm(
    root: &GlobalScope,
    bytes: Vec<u8>,
    mime: &[u8],
) -> Fallible<FetchedData> {
    let mime_string = if let Ok(s) = String::from_utf8(mime.to_vec()) {
        s
    } else {
        "".to_string()
    };
    let blob = Blob::new(root, BlobImpl::new_from_bytes(bytes), mime_string);
    Ok(FetchedData::BlobData(blob))
}

fn run_form_data_algorithm(
    root: &GlobalScope,
    bytes: Vec<u8>,
    mime: &[u8],
) -> Fallible<FetchedData> {
    let mime_str = if let Ok(s) = str::from_utf8(mime) {
        s
    } else {
        ""
    };
    let mime: Mime = mime_str
        .parse()
        .map_err(|_| Error::Type("Inappropriate MIME-type for Body".to_string()))?;

    // TODO
    // ... Parser for Mime(TopLevel::Multipart, SubLevel::FormData, _)
    // ... is not fully determined yet.
    if mime.type_() == mime::APPLICATION && mime.subtype() == mime::WWW_FORM_URLENCODED {
        let entries = form_urlencoded::parse(&bytes);
        let formdata = FormData::new(None, root);
        for (k, e) in entries {
            formdata.Append(USVString(k.into_owned()), USVString(e.into_owned()));
        }
        return Ok(FetchedData::FormData(formdata));
    }

    Err(Error::Type("Inappropriate MIME-type for Body".to_string()))
}

#[allow(unsafe_code)]
unsafe fn run_array_buffer_data_algorithm(
    cx: *mut JSContext,
    bytes: Vec<u8>,
) -> Fallible<FetchedData> {
    rooted!(in(cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
    let arraybuffer =
        ArrayBuffer::create(cx, CreateWith::Slice(&bytes), array_buffer_ptr.handle_mut());
    if arraybuffer.is_err() {
        return Err(Error::JSFailed);
    }
    let rooted_heap = RootedTraceableBox::from_box(Heap::boxed(array_buffer_ptr.get()));
    Ok(FetchedData::ArrayBuffer(rooted_heap))
}

pub trait BodyOperations {
    fn get_body_used(&self) -> bool;
    fn set_body_promise(&self, p: &Rc<Promise>, body_type: BodyType);
    /// Returns `Some(_)` if the body is complete, `None` if there is more to
    /// come.
    fn take_body(&self) -> Option<Vec<u8>>;
    fn is_locked(&self) -> bool;
    fn get_mime_type(&self) -> Ref<Vec<u8>>;
}
