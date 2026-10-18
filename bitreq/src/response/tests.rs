use super::*;

#[cfg(feature = "std")]
fn sync_response(bytes: &[u8], head: bool, limit: Option<usize>) -> Result<Response, Error> {
    let stream = HttpStream::create_buffer(bytes.to_vec());
    let lazy = ResponseLazy::from_stream(stream, None, None, limit)?;
    Response::create(lazy, head, limit)
}

#[cfg(feature = "std")]
#[test]
fn oversized_content_length_is_rejected_without_body_bytes() {
    for status in [200, 500] {
        let bytes = format!("HTTP/1.1 {status} Test\r\nContent-Length: 7\r\n\r\n");
        assert!(matches!(
            sync_response(bytes.as_bytes(), false, Some(6)),
            Err(Error::BodyOverflow)
        ));
        let stream = HttpStream::create_buffer(bytes.into_bytes());
        let mut lazy = ResponseLazy::from_stream(stream, None, None, Some(6)).unwrap();
        assert!(matches!(lazy.next(), Some(Err(Error::BodyOverflow))));
    }
}

#[cfg(feature = "std")]
#[test]
fn content_length_limit_respects_response_framing() {
    let exact = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n123456";
    for limit in [None, Some(6)] {
        assert_eq!(sync_response(exact, false, limit).unwrap().into_bytes(), b"123456");
    }
    for (status, head) in [(200, true), (204, false), (304, false)] {
        let bytes = format!("HTTP/1.1 {status} Test\r\nContent-Length: 7\r\n\r\n");
        assert!(sync_response(bytes.as_bytes(), head, Some(6)).unwrap().into_bytes().is_empty());
    }
    let chunked = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\n\r\n";
    assert_eq!(sync_response(chunked, false, Some(6)).unwrap().into_bytes(), b"a");
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_oversized_content_length_is_rejected_without_body_bytes() {
    for status in [200, 500] {
        let bytes = format!("HTTP/1.1 {status} Test\r\nContent-Length: 7\r\n\r\n");
        let result = Response::create_async(bytes.as_bytes(), false, None, None, Some(6)).await;
        assert!(matches!(result, Err(Error::BodyOverflow)));
    }
}

#[cfg(feature = "async")]
#[tokio::test]
async fn async_content_length_limit_respects_response_framing() {
    let exact = b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n123456";
    for limit in [None, Some(6)] {
        let response =
            Response::create_async(exact.as_slice(), false, None, None, limit).await.unwrap();
        assert_eq!(response.into_bytes(), b"123456");
    }
    for (status, head) in [(200, true), (204, false), (304, false)] {
        let bytes = format!("HTTP/1.1 {status} Test\r\nContent-Length: 7\r\n\r\n");
        let response =
            Response::create_async(bytes.as_bytes(), head, None, None, Some(6)).await.unwrap();
        assert!(response.into_bytes().is_empty());
    }
    let chunked = b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\n\r\n";
    let response =
        Response::create_async(chunked.as_slice(), false, None, None, Some(6)).await.unwrap();
    assert_eq!(response.into_bytes(), b"a");
}
