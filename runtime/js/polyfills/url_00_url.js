function parseUrlEncoded(bytes) {
  return ops.op_url_parse_search_params(null, bytes);
}

const URL = window.URL;
const URLPrototype = window.URL.prototype;
const URLSearchParams = window.URLSearchParams;
const URLSearchParamsPrototype = window.URLSearchParams.prototype;

export {
  URL,
  URLPrototype,
  URLSearchParams,
  URLSearchParamsPrototype,
  parseUrlEncoded,
};
