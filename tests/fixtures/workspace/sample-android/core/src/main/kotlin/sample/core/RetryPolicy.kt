package sample.core

/**
 * Decides how long to wait before the next attempt of a failed operation.
 * The delay doubles on each attempt and never exceeds [maxDelayMs].
 */
class RetryPolicy(
    val maxAttempts: Int = 3,
    private val baseDelayMs: Long = 200,
    private val maxDelayMs: Long = 5_000,
) {
    fun shouldRetry(attempt: Int): Boolean = attempt < maxAttempts

    fun delayFor(attempt: Int): Long {
        val exponential = baseDelayMs shl attempt.coerceAtMost(16)
        return exponential.coerceAtMost(maxDelayMs)
    }
}
