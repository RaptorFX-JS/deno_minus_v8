const primordials = globalThis.__bootstrap.primordials;
const {
  SymbolToStringTag,
  ArrayPrototypeIncludes,
  Proxy,
  ReflectGet,
  ReflectGetOwnPropertyDescriptor,
  ReflectGetPrototypeOf,
} = primordials;

/** Creates a proxy that represents a subset of the properties
 * of the original object optionally without evaluating the properties
 * in order to get the values. */
function createFilteredInspectProxy({ object, keys, evaluate }) {
  return new Proxy({}, {
    get(_target, key) {
      if (key === SymbolToStringTag) {
        return object.constructor?.name;
      } else if (ArrayPrototypeIncludes(keys, key)) {
        return ReflectGet(object, key);
      } else {
        return undefined;
      }
    },
    getOwnPropertyDescriptor(_target, key) {
      if (!ArrayPrototypeIncludes(keys, key)) {
        return undefined;
      } else if (evaluate) {
        return getEvaluatedDescriptor(object, key);
      } else {
        return getDescendantPropertyDescriptor(object, key) ??
          getEvaluatedDescriptor(object, key);
      }
    },
    has(_target, key) {
      return ArrayPrototypeIncludes(keys, key);
    },
    ownKeys() {
      return keys;
    },
  });

  function getDescendantPropertyDescriptor(object, key) {
    let propertyDescriptor = ReflectGetOwnPropertyDescriptor(object, key);
    if (!propertyDescriptor) {
      const prototype = ReflectGetPrototypeOf(object);
      if (prototype) {
        propertyDescriptor = getDescendantPropertyDescriptor(prototype, key);
      }
    }
    return propertyDescriptor;
  }

  function getEvaluatedDescriptor(object, key) {
    return {
      configurable: true,
      enumerable: true,
      value: object[key],
    };
  }
}

export { createFilteredInspectProxy };
