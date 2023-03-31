// minus_v8 specific script. polyfills internal implementation details of Deno's web APIs
"use strict";

// from ext/url/00_url.js
((window) => {
  /**
   * This function implements application/x-www-form-urlencoded parsing.
   * https://url.spec.whatwg.org/#concept-urlencoded-parser
   * @param {Uint8Array} bytes
   * @returns {[string, string][]}
   */
  function parseUrlEncoded(bytes) {
    return core.opSync("op_url_parse_search_params", null, bytes);
  }

  window.__bootstrap.url = {
    URL: window.URL,
    URLPrototype: window.URL.prototype,
    URLSearchParams: window.URLSearchParams,
    URLSearchParamsPrototype: window.URLSearchParams.prototype,
    parseUrlEncoded,
  };
})(this);

// from ext/web/01_dom_exception.js
((window) => {
  window.__bootstrap.domException = {
    DOMException: window.DOMException,
  };
})(this);

// from ext/web/02_event.js
((window) => {
  const {
    FunctionPrototypeCall,
    Map,
    MapPrototypeGet,
    MapPrototypeSet,
    ObjectDefineProperty,
    ObjectPrototypeIsPrototypeOf,
    Symbol,
  } = window.__bootstrap.primordials;

  function setEventTargetData() {
    // runtime should magically set up event target data by itself
  }

  const _eventHandlers = Symbol("eventHandlers");

  function makeWrappedHandler(handler, isSpecialErrorEventHandler) {
    function wrappedHandler(evt) {
      if (typeof wrappedHandler.handler !== "function") {
        return;
      }

      if (
        isSpecialErrorEventHandler &&
        ObjectPrototypeIsPrototypeOf(ErrorEvent.prototype, evt) &&
        evt.type === "error"
      ) {
        const ret = FunctionPrototypeCall(
          wrappedHandler.handler,
          this,
          evt.message,
          evt.filename,
          evt.lineno,
          evt.colno,
          evt.error,
        );
        if (ret === true) {
          evt.preventDefault();
        }
        return;
      }

      return FunctionPrototypeCall(wrappedHandler.handler, this, evt);
    }
    wrappedHandler.handler = handler;
    return wrappedHandler;
  }

  // `init` is an optional function that will be called the first time that the
  // event handler property is set. It will be called with the object on which
  // the property is set as its argument.
  // `isSpecialErrorEventHandler` can be set to true to opt into the special
  // behavior of event handlers for the "error" event in a global scope.
  function defineEventHandler(
    emitter,
    name,
    init = undefined,
    isSpecialErrorEventHandler = false,
  ) {
    // HTML specification section 8.1.7.1
    ObjectDefineProperty(emitter, `on${name}`, {
      get() {
        if (!this[_eventHandlers]) {
          return null;
        }

        return MapPrototypeGet(this[_eventHandlers], name)?.handler ?? null;
      },
      set(value) {
        // All three Web IDL event handler types are nullable callback functions
        // with the [LegacyTreatNonObjectAsNull] extended attribute, meaning
        // anything other than an object is treated as null.
        if (typeof value !== "object" && typeof value !== "function") {
          value = null;
        }

        if (!this[_eventHandlers]) {
          this[_eventHandlers] = new Map();
        }
        let handlerWrapper = MapPrototypeGet(this[_eventHandlers], name);
        if (handlerWrapper) {
          handlerWrapper.handler = value;
        } else if (value !== null) {
          handlerWrapper = makeWrappedHandler(
            value,
            isSpecialErrorEventHandler,
          );
          this.addEventListener(name, handlerWrapper);
          init?.(this);
        }
        MapPrototypeSet(this[_eventHandlers], name, handlerWrapper);
      },
      configurable: true,
      enumerable: true,
    });
  }

  window.__bootstrap.eventTarget = {
    setEventTargetData,
  };
  window.__bootstrap.event = {
    defineEventHandler,
  };
})(this);

// from ext/web/03_abort_signal.js
((window) => {
  const {
    ObjectAssign,
    Set,
    SetPrototypeAdd,
    SetPrototypeDelete,
    Symbol,
  } = globalThis.__bootstrap.primordials;

  const add = Symbol("[[add]]");
  const signalAbort = Symbol("[[signalAbort]]");
  const remove = Symbol("[[remove]]");
  const abortAlgos = Symbol("[[abortAlgos]]");

  window.AbortController = new Proxy(window.AbortController, {
    construct: (target, args) => {
      const controller = new target(...args);
      const signal = controller.signal;
      ObjectAssign(signal, {
        [abortAlgos]: null,

        [add](algorithm) {
          if (this.aborted) {
            return;
          }
          if (this[abortAlgos] === null) {
            this[abortAlgos] = new Set();
          }
          SetPrototypeAdd(this[abortAlgos], algorithm);
        },

        [signalAbort](
          reason = new DOMException("The signal has been aborted", "AbortError"),
        ) {
          controller.abort(reason);
        },

        [remove](algorithm) {
          this[abortAlgos] && SetPrototypeDelete(this[abortAlgos], algorithm);
        },
      });
      signal.addEventListener("abort", () => {
        if (signal[abortAlgos] !== null) {
          for (const algorithm of signal[abortAlgos]) {
            algorithm();
          }
          signal[abortAlgos] = null;
        }
      });
      return controller;
    }
  });

  function newSignal() {
    return new window.AbortController().signal;
  }

  function follow(followingSignal, parentSignal) {
    if (followingSignal.aborted) {
      return;
    }
    if (parentSignal.aborted) {
      followingSignal[signalAbort](parentSignal.reason);
    } else {
      parentSignal[add](() =>
        followingSignal[signalAbort](parentSignal.reason)
      );
    }
  }

  window.__bootstrap.abortSignal = {
    AbortSignalPrototype: window.AbortSignal.prototype,
    add,
    signalAbort,
    remove,
    follow,
    newSignal,
  }
})(this);

// from ext/web/06_streams.js
((window) => {
  const core = window.Deno.core;
  const {
    ObjectAssign,
    Promise,
    Proxy,
    Symbol,
  } = globalThis.__bootstrap.primordials;

  /** @template T */
  class Deferred {
    /** @type {Promise<T>} */
    #promise;
    /** @type {(reject?: any) => void} */
    #reject;
    /** @type {(value: T | PromiseLike<T>) => void} */
    #resolve;
    /** @type {"pending" | "fulfilled"} */
    #state = "pending";

    constructor() {
      this.#promise = new Promise((resolve, reject) => {
        this.#resolve = resolve;
        this.#reject = reject;
      });
    }

    /** @returns {Promise<T>} */
    get promise() {
      return this.#promise;
    }

    /** @returns {"pending" | "fulfilled"} */
    get state() {
      return this.#state;
    }

    /** @param {any=} reason */
    reject(reason) {
      // already settled promises are a no-op
      if (this.#state !== "pending") {
        return;
      }
      this.#state = "fulfilled";
      this.#reject(reason);
    }

    /** @param {T | PromiseLike<T>} value */
    resolve(value) {
      // already settled promises are a no-op
      if (this.#state !== "pending") {
        return;
      }
      this.#state = "fulfilled";
      this.#resolve(value);
    }
  }

  const _controller = Symbol("[[controller]]");
  const _maybeRid = Symbol("[[maybeRid]]");

  const DEFAULT_CHUNK_SIZE = 64 * 1024; // 64 KiB

  window.ReadableStream = new Proxy(window.ReadableStream, {
    construct: (target, args) => {
      let controller;
      const injectedStart = c => {
        controller = c;
      }
      if (args.length === 0 || args[0] === null || args[0] === undefined) {
        args[0] = {start: injectedStart};
      } else {
        let origStart = args[0].start;
        ObjectAssign(args[0], {
          start(c) {
            injectedStart(c);
            if (origStart) {
              origStart(c);
            }
          }
        });
      }
      let out = new target(...args);
      out[_controller] = controller;
      return out;
    }
  });

  /**
   * @param {ReadableStream} stream
   * @returns {boolean}
   */
  function isReadableStreamDisturbed(stream) {
    // see https://github.com/whatwg/streams/issues/1025
    try {
      new window.Response(stream);
      return false;
    } catch {
      return true;
    }
  }

  /**
   * @callback unrefCallback
   * @param {Promise} promise
   * @returns {undefined}
   */
  /**
   * @param {number} rid
   * @param {unrefCallback=} unrefCallback
   * @returns {ReadableStream<Uint8Array>}
   */
  function readableStreamForRid(rid, unrefCallback) {
    const stream = new window.ReadableStream({
      type: "bytes",
      async pull(controller) {
        const v = controller.byobRequest.view;
        try {
          const promise = core.read(rid, v);

          unrefCallback?.(promise);

          const bytesRead = await promise;

          if (bytesRead === 0) {
            core.tryClose(rid);
            controller.close();
            controller.byobRequest.respond(0);
          } else {
            controller.byobRequest.respond(bytesRead);
          }
        } catch (e) {
          controller.error(e);
          core.tryClose(rid);
        }
      },
      cancel() {
        core.tryClose(rid);
      },
      autoAllocateChunkSize: DEFAULT_CHUNK_SIZE,
    });

    stream[_maybeRid] = rid;
    return stream;
  }

  function getReadableStreamRid(stream) {
    return stream[_maybeRid];
  }

  /**
   * @template R
   * @param {ReadableStream<R>} stream
   * @returns {void}
   */
  function readableStreamClose(stream) {
    stream[_controller].close();
  }

  function errorReadableStream(stream, e) {
    stream[_controller].error(e);
  }

  /**
   * @param {WritableStream} stream
   * @returns {Promise<void>}
   */
  function writableStreamClose(stream) {
    return stream.close();
  }

  /**
   * @param {ReadableStream} stream
   */
  function createProxy(stream) {
    return stream.pipeThrough(new window.TransformStream());
  }

  window.__bootstrap.streams = {
    isReadableStreamDisturbed,
    errorReadableStream,
    createProxy,
    writableStreamClose,
    readableStreamClose,
    readableStreamForRid,
    getReadableStreamRid,
    Deferred,
    ReadableStream: window.ReadableStream,
    ReadableStreamPrototype: window.ReadableStream.prototype,
    WritableStream: window.WritableStream,
    WritableStreamPrototype: window.WritableStream.prototype,
  };
})(this);

// from ext/web/09_file.js
((window) => {
  const {StringPrototypeStartsWith} = window.__bootstrap.primordials;
  const XMLHttpRequest = window.XMLHttpRequest;

  /**
   * Construct a new Blob object from an object URL.
   *
   * This new object will not duplicate data in memory with the original Blob
   * object from which this URL was created or with other Blob objects created
   * from the same URL, but they will be different objects.
   *
   * The object returned from this function will not be a File object, even if
   * the original object from which the object URL was constructed was one. This
   * means that the `name` and `lastModified` properties are lost.
   *
   * @param {string} url
   * @returns {Blob | null}
   */
  // TODO(minus_v8) This will not work in a Firefox-based browser
  function blobFromObjectUrl(url) {
    if (!StringPrototypeStartsWith(url, "blob:")) {
      return null;
    }
    let blob;
    let xhr = new XMLHttpRequest();
    xhr.open("GET", url, false);
    xhr.responseType = "blob";
    xhr.onload = function () {
      if (this.status === 200) {
        blob = this.response;
      }
    };
    xhr.send();
    return blob;
  }

  window.__bootstrap.file = {
    blobFromObjectUrl,
    Blob: window.Blob,
    BlobPrototype: window.Blob.prototype,
    File: window.File,
    FilePrototype: window.File.prototype,
  };
})(this);

// from ext/web/12_location.js
((window) => {
  window.__bootstrap.location = {
    setLocationHref(href) {
      // used only in 99_main.js and not needed
    },
    getLocationHref() {
      return window.location?.href;
    },
  }
})(this);
