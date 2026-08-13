declare global {
  namespace Cloudflare {
    interface Env {
      BRAIN: DurableObjectNamespace<import('../src/brain.js').AndaBrain>
    }
  }
}

export {}
