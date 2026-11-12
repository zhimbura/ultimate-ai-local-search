package sample.network

import sample.core.RetryPolicy

data class RemoteConfig(val values: Map<String, String>)

fun interface HttpGet {
    fun get(url: String): String
}

/**
 * Downloads the remote config. On a network error it waits according to
 * [RetryPolicy] and tries again on the next host from [HostSelector].
 */
class ConfigLoader(
    private val selector: HostSelector,
    private val retry: RetryPolicy,
    private val http: HttpGet,
) {
    fun load(): RemoteConfig {
        var attempt = 0
        while (true) {
            val host = selector.pick()
            try {
                return parse(http.get("$host/config"))
            } catch (error: Exception) {
                selector.markFailed(host)
                if (!retry.shouldRetry(attempt)) {
                    throw error
                }
                Thread.sleep(retry.delayFor(attempt))
                attempt++
            }
        }
    }

    private fun parse(body: String): RemoteConfig =
        RemoteConfig(
            body.lines()
                .filter { "=" in it }
                .associate { line -> line.substringBefore("=") to line.substringAfter("=") },
        )
}
