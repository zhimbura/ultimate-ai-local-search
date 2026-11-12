package sample.app

import sample.core.LogAnalytics
import sample.core.RetryPolicy
import sample.network.ConfigLoader
import sample.network.HostSelector
import sample.network.HttpGet
import sample.network.InMemoryHostStore

fun main() {
    val selector = HostSelector(
        defaults = listOf("https://api.example.com", "https://backup.example.com"),
        store = InMemoryHostStore(),
        analytics = LogAnalytics(),
    )
    val loader = ConfigLoader(selector, RetryPolicy(), HttpGet { url -> error("offline: $url") })
    println(runCatching { loader.load() })
}
