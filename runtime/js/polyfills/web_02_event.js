const {
  FunctionPrototypeCall,
  Map,
  MapPrototypeGet,
  MapPrototypeSet,
  ObjectDefineProperty,
  ObjectPrototypeIsPrototypeOf,
  Symbol,
} = window.__bootstrap.primordials;

const _skipInternalInit = Symbol("[[skipSlowInit]]");
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

const CloseEvent = window.CloseEvent;
const CustomEvent = window.CustomEvent;
const ErrorEvent = window.ErrorEvent;
const Event = window.Event;
const EventTarget = window.EventTarget;
const MessageEvent = window.MessageEvent;
const ProgressEvent = window.ProgressEvent;
const PromiseRejectionEvent = window.PromiseRejectionEvent;

export {
  _skipInternalInit,
  CloseEvent,
  CustomEvent,
  defineEventHandler,
  ErrorEvent,
  Event,
  EventTarget,
  MessageEvent,
  ProgressEvent,
  PromiseRejectionEvent,
  setEventTargetData,
};
