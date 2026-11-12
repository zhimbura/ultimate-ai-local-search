package sample.core

/** Sends product analytics events. */
interface Analytics {
    fun track(name: String, params: Map<String, String> = emptyMap())
}

class LogAnalytics : Analytics {
    override fun track(name: String, params: Map<String, String>) {
        println("event $name $params")
    }
}
