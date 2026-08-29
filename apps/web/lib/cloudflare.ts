import { getRequestContext } from '@cloudflare/next-on-pages'

export function bindings(): CloudflareEnv {
  return getRequestContext().env
}

export function originFor(requestUrl: string): string {
  const configured = bindings().PUBLIC_ORIGIN?.replace(/\/$/, '')
  return configured || new URL(requestUrl).origin
}
