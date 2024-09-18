let ReactDOMServer

try {
  ReactDOMServer = require('react-dom/server.edge')
} catch (error) {
  if (
    // TODO: copilot suggestion. Does this code actually exist?
    error.code !== 'MODULE_NOT_FOUND' &&
    // TODO: actually encountered that
    error.code !== 'ERR_PACKAGE_PATH_NOT_EXPORTED'
  ) {
    throw error
  }
  // TODO: Ensure App Router does not bundle this
  // In React versions without react-dom/server.edge, the browser build works in Node.js.
  // The Node.js build does not support renderToReadableStream.
  ReactDOMServer = require('react-dom/server.browser')
}

module.exports = ReactDOMServer
