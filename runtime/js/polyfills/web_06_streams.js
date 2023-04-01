// TODO(minus_v8) the following code will not work on WebKit since the BYOB buffers are not implemented there

const core = window.Deno.core;
const {
  ObjectAssign,
  ObjectPrototypeIsPrototypeOf,
  FinalizationRegistry,
  Promise,
  Proxy,
  Symbol,
  TypedArrayPrototypeSet,
  Uint8ArrayPrototype,
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
const _resourceBacking = Symbol("[[resourceBacking]]");
// This distinction exists to prevent unrefable streams being used in
// regular fast streams that are unaware of refability
const _resourceBackingUnrefable = Symbol("[[resourceBackingUnrefable]]");

const DEFAULT_CHUNK_SIZE = 64 * 1024; // 64 KiB

// A finalization registry to clean up underlying resources that are GC'ed.
const RESOURCE_REGISTRY = new FinalizationRegistry((rid) => {
  core.tryClose(rid);
});

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

const Response = window.Response;
const ReadableStream = window.ReadableStream;
const ReadableStreamPrototype = window.ReadableStream.prototype;
const WritableStream = window.WritableStream;
const WritableStreamPrototype = window.WritableStream.prototype;

/**
 * @param {ReadableStream} stream
 * @returns {boolean}
 */
function isReadableStreamDisturbed(stream) {
  // see https://github.com/whatwg/streams/issues/1025
  try {
    new Response(stream);
    return false;
  } catch {
    return true;
  }
}

/**
 * Create a new ReadableStream object that is backed by a Resource that
 * implements `Resource::read_return`. This object contains enough metadata to
 * allow callers to bypass the JavaScript ReadableStream implementation and
 * read directly from the underlying resource if they so choose (FastStream).
 *
 * @param {number} rid The resource ID to read from.
 * @param {boolean=} autoClose If the resource should be auto-closed when the stream closes. Defaults to true.
 * @returns {ReadableStream<Uint8Array>}
 */
function readableStreamForRid(rid, autoClose = true) {
  const tryClose = () => {
    if (!autoClose) return;
    RESOURCE_REGISTRY.unregister(stream);
    core.tryClose(rid);
  };

  const stream = new ReadableStream({
    type: "bytes",
    async pull(controller) {
      const v = controller.byobRequest.view;
      try {
        const bytesRead = await core.read(rid, v);
        if (bytesRead === 0) {
          tryClose();
          controller.close();
          controller.byobRequest.respond(0);
        } else {
          controller.byobRequest.respond(bytesRead);
        }
      } catch (e) {
        controller.error(e);
        tryClose();
      }
    },
    cancel() {
      tryClose();
    },
    autoAllocateChunkSize: DEFAULT_CHUNK_SIZE,
  });

  stream[_resourceBacking] = { rid, autoClose };

  if (autoClose) {
    RESOURCE_REGISTRY.register(stream, rid, stream);
  }

  return stream;
}

const promiseIdSymbol = SymbolFor("Deno.core.internalPromiseId");
const _isUnref = Symbol("isUnref");

/**
 * Create a new ReadableStream object that is backed by a Resource that
 * implements `Resource::read_return`. This readable stream supports being
 * refed and unrefed by calling `readableStreamForRidUnrefableRef` and
 * `readableStreamForRidUnrefableUnref` on it. Unrefable streams are not
 * FastStream compatible.
 *
 * @param {number} rid The resource ID to read from.
 * @returns {ReadableStream<Uint8Array>}
 */
function readableStreamForRidUnrefable(rid) {
  const stream = new ReadableStream({
    type: "bytes",
    async pull(controller) {
      const v = controller.byobRequest.view;
      try {
        const promise = core.read(rid, v);
        const promiseId = stream[promiseIdSymbol] = promise[promiseIdSymbol];
        if (stream[_isUnref]) core.unrefOp(promiseId);
        const bytesRead = await promise;
        stream[promiseIdSymbol] = undefined;
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

  stream[promiseIdSymbol] = undefined;
  stream[_isUnref] = false;
  stream[_resourceBackingUnrefable] = { rid, autoClose: true };
  return stream;
}

function readableStreamIsUnrefable(stream) {
  return ReflectHas(stream, _isUnref);
}

function readableStreamForRidUnrefableRef(stream) {
  if (!readableStreamIsUnrefable(stream)) {
    throw new TypeError("Not an unrefable stream");
  }
  stream[_isUnref] = false;
  if (stream[promiseIdSymbol] !== undefined) {
    core.refOp(stream[promiseIdSymbol]);
  }
}

function readableStreamForRidUnrefableUnref(stream) {
  if (!readableStreamIsUnrefable(stream)) {
    throw new TypeError("Not an unrefable stream");
  }
  stream[_isUnref] = true;
  if (stream[promiseIdSymbol] !== undefined) {
    core.unrefOp(stream[promiseIdSymbol]);
  }
}

function getReadableStreamResourceBacking(stream) {
  return stream[_resourceBacking];
}

function getReadableStreamResourceBackingUnrefable(stream) {
  return stream[_resourceBackingUnrefable];
}

async function readableStreamCollectIntoUint8Array(stream) {
  const resourceBacking = getReadableStreamResourceBacking(stream) ||
    getReadableStreamResourceBackingUnrefable(stream);
  const reader = stream.getReader();

  if (resourceBacking) {
    // TODO(minus_v8) this does not properly handle already error'd streams and does not always properly disturb the stream
    // fast path, read whole body in a single op call
    try {
      const promise = core.opAsync("op_read_all", resourceBacking.rid);
      if (readableStreamIsUnrefable(stream)) {
        const promiseId = stream[promiseIdSymbol] = promise[promiseIdSymbol];
        if (stream[_isUnref]) core.unrefOp(promiseId);
      }
      const buf = await promise;
      readableStreamClose(stream);
      return buf;
    } catch (err) {
      errorReadableStream(stream, err);
      throw err;
    } finally {
      if (resourceBacking.autoClose) {
        core.tryClose(resourceBacking.rid);
      }
    }
  }

  // slow path
  /** @type {Uint8Array[]} */
  const chunks = [];
  let totalLength = 0;

  while (true) {
    const { value: chunk, done } = await reader.read();

    if (done) break;

    if (!ObjectPrototypeIsPrototypeOf(Uint8ArrayPrototype, chunk)) {
      throw new TypeError(
        "Can't convert value to Uint8Array while consuming the stream",
      );
    }

    ArrayPrototypePush(chunks, chunk);
    totalLength += chunk.byteLength;
  }

  const finalBuffer = new Uint8Array(totalLength);
  let offset = 0;
  for (let i = 0; i < chunks.length; ++i) {
    const chunk = chunks[i];
    TypedArrayPrototypeSet(finalBuffer, chunk, offset);
    offset += chunk.byteLength;
  }
  return finalBuffer;
}

/*
 * @param {ReadableStream} stream
 */
function readableStreamThrowIfErrored(stream) {
  // TODO(minus_v8) this may be impossible to implement without side effects
  //                but since the only usage of this in 22_body.js immediately
  //                is followed by readableStreamCollectIntoUint8Array, we
  //                probably don't need to impl this
}

/**
 * @template R
 * @param {ReadableStream<R>} stream
 * @returns {void}
 */
function readableStreamDisturb(stream) {
  // TODO(minus_v8) this may be impossible to implement without side effects,
  //                but since the only usage of this in 22_body.js immediately
  //                is followed by close, no impl is needed
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

export {
  createProxy,
  Deferred,
  errorReadableStream,
  getReadableStreamResourceBacking,
  isReadableStreamDisturbed,
  ReadableStream,
  readableStreamClose,
  readableStreamCollectIntoUint8Array,
  readableStreamDisturb,
  readableStreamForRid,
  readableStreamForRidUnrefable,
  readableStreamForRidUnrefableRef,
  readableStreamForRidUnrefableUnref,
  ReadableStreamPrototype,
  readableStreamThrowIfErrored,
  WritableStream,
  writableStreamClose,
};
