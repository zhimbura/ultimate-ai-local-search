package sample.core

import kotlin.test.Test
import kotlin.test.assertEquals

class RetryPolicyTest {
    @Test
    fun delayDoublesAndIsCapped() {
        val policy = RetryPolicy(maxAttempts = 5, baseDelayMs = 100, maxDelayMs = 300)
        assertEquals(100, policy.delayFor(0))
        assertEquals(200, policy.delayFor(1))
        assertEquals(300, policy.delayFor(2))
    }
}
