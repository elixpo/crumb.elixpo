import { initOpenNextCloudflareForDev } from '@opennextjs/cloudflare'
import nextEnv from '@next/env'
import { fileURLToPath } from 'node:url'

const { loadEnvConfig } = nextEnv
loadEnvConfig(fileURLToPath(new URL('..', import.meta.url)), process.env.NODE_ENV !== 'production', console, true)

initOpenNextCloudflareForDev()

/** @type {import('next').NextConfig} */
export default { experimental: {} }
