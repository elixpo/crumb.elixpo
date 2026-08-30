import { getCloudflareContext } from '@opennextjs/cloudflare'

export function bindings(): CloudflareEnv {
  return getCloudflareContext().env
}

export function config() {
  const env = bindings()
  return {
    accountsOrigin: env.ACCOUNTS_ORIGIN || process.env.ACCOUNTS_ORIGIN || 'https://accounts.elixpo.com',
    accountsClientId: env.NEXT_PUBLIC_ELIXPO_CLIENT_ID || process.env.NEXT_PUBLIC_ELIXPO_CLIENT_ID || '',
    accountsCliClientId: env.NEXT_PUBLIC_ELIXPO_CLIENT_ID_CLI || process.env.NEXT_PUBLIC_ELIXPO_CLIENT_ID_CLI || '',
    accountsClientSecret: env.ELIXPO_CLIENT_SECRET || process.env.ELIXPO_CLIENT_SECRET || '',
    accountsWebhookSecret: env.ELIXPO_ACCOUNTS_WEBHOOK_SECRET || process.env.ELIXPO_ACCOUNTS_WEBHOOK_SECRET || '',
    accountsCliAudience: env.ACCOUNTS_CLI_AUDIENCE || process.env.ACCOUNTS_CLI_AUDIENCE || 'crumb.elixpo.com',
    pollinationsAppKey: env.POLLINATIONS_APP_KEY || process.env.POLLINATIONS_APP_KEY || '',
    connectorEncryptionKey: env.CONNECTOR_ENCRYPTION_KEY || process.env.CONNECTOR_ENCRYPTION_KEY || '',
  }
}

export function originFor(requestUrl: string): string {
  return new URL(requestUrl).origin
}
