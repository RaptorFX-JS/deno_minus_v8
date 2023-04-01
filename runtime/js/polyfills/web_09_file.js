const { StringPrototypeStartsWith } = window.__bootstrap.primordials;
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

const Blob = window.Blob;
const BlobPrototype = window.Blob.prototype;
const File = window.File;
const FilePrototype = window.File.prototype;

export {
  blobFromObjectUrl,
  Blob,
  BlobPrototype,
  File,
  FilePrototype,
};
