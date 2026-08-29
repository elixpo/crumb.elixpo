interface CloudflareEnv {
  DB: D1Database
  KV: KVNamespace
  NEXT_PUBLIC_ELIXPO_CLIENT_ID: string
  ELIXPO_CLIENT_SECRET: string
  POLLINATIONS_APP_KEY: string
  CONNECTOR_ENCRYPTION_KEY: string
  ACCOUNTS_ORIGIN: string
}

declare module '@cloudflare/next-on-pages' {
  export function getRequestContext(): {
    env: CloudflareEnv
    ctx: ExecutionContext
  }
}
