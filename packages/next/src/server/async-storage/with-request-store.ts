import type { BaseNextRequest, BaseNextResponse } from '../base-http'
import type { IncomingHttpHeaders } from 'http'
import type { AsyncLocalStorage } from 'async_hooks'
import type { RequestStore } from '../../client/components/request-async-storage.external'
import type { RenderOpts } from '../app-render/types'
import type { WithStore } from './with-store'
import type { NextRequest } from '../web/spec-extension/request'
import type { __ApiPreviewProps } from '../api-utils'

import { FLIGHT_HEADERS } from '../../client/components/app-router-headers'
import {
  HeadersAdapter,
  type ReadonlyHeaders,
} from '../web/spec-extension/adapters/headers'
import {
  MutableRequestCookiesAdapter,
  RequestCookiesAdapter,
  type ReadonlyRequestCookies,
} from '../web/spec-extension/adapters/request-cookies'
import { ResponseCookies, RequestCookies } from '../web/spec-extension/cookies'
import { DraftModeProvider } from './draft-mode-provider'
import { splitCookiesString } from '../web/utils'
import { AfterContext } from '../after/after-context'
import type { RequestLifecycleOpts } from '../base-server'
import type { ServerComponentsHmrCache } from '../response-cache'

function getHeaders(headers: Headers | IncomingHttpHeaders): ReadonlyHeaders {
  const cleaned = HeadersAdapter.from(headers)
  for (const header of FLIGHT_HEADERS) {
    cleaned.delete(header.toLowerCase())
  }

  return HeadersAdapter.seal(cleaned)
}

function getMutableCookies(
  headers: Headers | IncomingHttpHeaders,
  onUpdateCookies?: (cookies: string[]) => void
): ResponseCookies {
  const cookies = new RequestCookies(HeadersAdapter.from(headers))
  return MutableRequestCookiesAdapter.wrap(cookies, onUpdateCookies)
}

export type WrapperRenderOpts = Partial<
  Pick<
    RenderOpts,
    'ComponentMod' | 'onUpdateCookies' | 'assetPrefix' | 'reactLoadableManifest'
  >
> & {
  experimental: Pick<RenderOpts['experimental'], 'after'>
  previewProps?: __ApiPreviewProps
}

export type RequestContext = RequestResponsePair & {
  /**
   * The URL of the request. This only specifies the pathname and the search
   * part of the URL. This is only undefined when generating static paths (ie,
   * there is no request in progress, nor do we know one).
   */
  url: {
    /**
     * The pathname of the requested URL.
     */
    pathname: string

    /**
     * The search part of the requested URL. If the request did not provide a
     * search part, this will be an empty string.
     */
    search?: string
  }
  renderOpts?: WrapperRenderOpts
  isHmrRefresh?: boolean
  serverComponentsHmrCache?: ServerComponentsHmrCache
}

type RequestResponsePair =
  | { req: BaseNextRequest; res: BaseNextResponse; context?: undefined } // for an app page
  | { req: NextRequest; res: undefined; context: Partial<RequestLifecycleOpts> } // in an api route or middleware

/**
 * If middleware set cookies in this request (indicated by `x-middleware-set-cookie`),
 * then merge those into the existing cookie object, so that when `cookies()` is accessed
 * it's able to read the newly set cookies.
 */
function mergeMiddlewareCookies(
  req: RequestContext['req'],
  existingCookies: RequestCookies | ResponseCookies
) {
  if (
    'x-middleware-set-cookie' in req.headers &&
    typeof req.headers['x-middleware-set-cookie'] === 'string'
  ) {
    const setCookieValue = req.headers['x-middleware-set-cookie']
    const responseHeaders = new Headers()

    for (const cookie of splitCookiesString(setCookieValue)) {
      responseHeaders.append('set-cookie', cookie)
    }

    const responseCookies = new ResponseCookies(responseHeaders)

    // Transfer cookies from ResponseCookies to RequestCookies
    for (const cookie of responseCookies.getAll()) {
      existingCookies.set(cookie)
    }
  }
}

export const withRequestStore: WithStore<RequestStore, RequestContext> = <
  Result,
>(
  storage: AsyncLocalStorage<RequestStore>,
  args: RequestContext,
  callback: (store: RequestStore) => Result
): Result => {
  const { req, url, res, renderOpts, isHmrRefresh, serverComponentsHmrCache } =
    args
  function defaultOnUpdateCookies(cookies: string[]) {
    if (res) {
      res.setHeader('Set-Cookie', cookies)
    }
  }

  const cache: {
    headers?: ReadonlyHeaders
    cookies?: ReadonlyRequestCookies
    mutableCookies?: ResponseCookies
    draftMode?: DraftModeProvider
  } = {}

  const store: RequestStore = {
    // Rather than just using the whole `url` here, we pull the parts we want
    // to ensure we don't use parts of the URL that we shouldn't. This also
    // lets us avoid requiring an empty string for `search` in the type.
    url: { pathname: url.pathname, search: url.search ?? '' },
    get headers() {
      if (!cache.headers) {
        // Seal the headers object that'll freeze out any methods that could
        // mutate the underlying data.
        cache.headers = getHeaders(req.headers)
      }

      return cache.headers
    },
    get cookies() {
      if (!cache.cookies) {
        // if middleware is setting cookie(s), then include those in
        // the initial cached cookies so they can be read in render
        const requestCookies = new RequestCookies(
          HeadersAdapter.from(req.headers)
        )

        mergeMiddlewareCookies(req, requestCookies)

        // Seal the cookies object that'll freeze out any methods that could
        // mutate the underlying data.
        cache.cookies = RequestCookiesAdapter.seal(requestCookies)
      }

      return cache.cookies
    },
    get mutableCookies() {
      if (!cache.mutableCookies) {
        const mutableCookies = getMutableCookies(
          req.headers,
          renderOpts?.onUpdateCookies ||
            (res ? defaultOnUpdateCookies : undefined)
        )

        mergeMiddlewareCookies(req, mutableCookies)

        cache.mutableCookies = mutableCookies
      }
      return cache.mutableCookies
    },
    get draftMode() {
      if (!cache.draftMode) {
        cache.draftMode = new DraftModeProvider(
          renderOpts?.previewProps,
          req,
          this.cookies,
          this.mutableCookies
        )
      }

      return cache.draftMode
    },

    reactLoadableManifest: renderOpts?.reactLoadableManifest || {},
    assetPrefix: renderOpts?.assetPrefix || '',
    afterContext: createAfterContext(args),
    isHmrRefresh,
    serverComponentsHmrCache:
      serverComponentsHmrCache ||
      (globalThis as any).__serverComponentsHmrCache,
  }

  if (store.afterContext) {
    return store.afterContext.run(store, () =>
      storage.run(store, callback, store)
    )
  }

  return storage.run(store, callback, store)
}

function createAfterContext(
  args: RequestResponsePair & Pick<RequestContext, 'renderOpts'>
): AfterContext | undefined {
  if (!isAfterEnabled(args.renderOpts)) {
    return undefined
  }

  if (args.context) {
    return new AfterContext({
      waitUntil: args.context.waitUntil,
      onClose: args.context.onClose,
    })
  }

  const { req, res } = args
  // For some reason, `req instanceof BaseNextRequest` is always false here,
  // so just check for the `context` property instead
  const waitUntil = 'context' in req ? req.context?.waitUntil : undefined

  // For some reason, `req instanceof BaseNextResponse` is always false here,
  // so just check for the `onClose` property instead
  const onClose = 'onClose' in res ? res.onClose.bind(res) : undefined

  return new AfterContext({ waitUntil, onClose })
}

function isAfterEnabled(
  renderOpts: WrapperRenderOpts | undefined
): renderOpts is WrapperRenderOpts {
  return renderOpts?.experimental?.after ?? false
}
