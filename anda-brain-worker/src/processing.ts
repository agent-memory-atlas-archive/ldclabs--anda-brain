/** Stable codes survive the Durable Object Error boundary as exact messages. */
export const PROCESSING_ERRORS = new Set([
  'memory_change_pending', 'memory_changed_rebuild_context', 'source_suppressed',
  'maintenance_busy', 'maintenance_run_expired',
])

export function processingError(code: string): never {
  throw new Error(code)
}

export function processingErrorCode(error: unknown): string | undefined {
  return error instanceof Error && PROCESSING_ERRORS.has(error.message) ? error.message : undefined
}
