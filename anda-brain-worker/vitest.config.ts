import { cloudflareTest } from '@cloudflare/vitest-pool-workers'
import { defineConfig } from 'vitest/config'

export default defineConfig({
  plugins: [
    cloudflareTest({
      isolatedStorage: false,
      wrangler: { configPath: './test/wrangler.jsonc' },
      miniflare: {
        compatibilityDate: '2026-08-08',
        compatibilityFlags: ['nodejs_compat'],
      },
    }),
  ],
})
