package io.tailsend.plugin.tsnet

import android.app.Activity
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import mobile.Mobile
import mobile.Node
import mobile.Prompt
import org.json.JSONArray
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

// Owns the process-wide tsnet node (gomobile AAR). The node survives webview
// reloads in the companion object; the always-on notification while the
// receiver runs is TailsendReceiverService's job. Rust calls in through
// run_mobile_plugin; transfer consent flows back through `pending`, which
// Rust polls and relays to the GUI as incoming-request events.
@TauriPlugin
class TsnetNodePlugin(private val activity: Activity) : Plugin(activity) {

  companion object {
    @Volatile
    var node: Node? = null
    val pending = ConcurrentHashMap<String, PendingAsk>()
    private val seq = AtomicLong()
    private val nodeLock = Any()
  }

  class PendingAsk(
    val id: String,
    val sender: String,
    val count: Long,
    val total: Long,
    val names: String,
    val future: CompletableFuture<String>,
  )

  private fun normalizeTarget(t: String): String =
    if (t.contains(':')) t else "$t:4782"

  private fun ensureNode(): Node {
    node?.let { existing ->
      // A node whose startup failed is terminal in Go — recreate on demand.
      val errored = try {
        org.json.JSONObject(existing.status()).optString("state", "") == "error"
      } catch (_: Exception) { false }
      if (!errored) return existing
      try { existing.shutdown() } catch (_: Exception) {}
      synchronized(nodeLock) { node = null }
    }
    synchronized(nodeLock) {
      node?.let { return it }
      val model = Build.MODEL?.trim().orEmpty().ifEmpty { "tailsend" }
      val host = model.replace(Regex("\\s+"), "-").take(40)
      @Suppress("DEPRECATION")
      val ver = try {
        activity.packageManager.getPackageInfo(activity.packageName, 0).versionName ?: "dev"
      } catch (_: Exception) {
        "dev"
      }
      val dir = activity.filesDir.absolutePath
      val n = Mobile.newNode(host, dir, ver)
      n.setPrompt(object : Prompt {
        // The Go gate enforces the real 30s deadline; 35s here is slack so
        // "timeout" answers come from Go, not from a JVM race.
        override fun ask(sender: String, count: Long, total: Long, namesJSON: String): String {
          val id = "n${seq.incrementAndGet()}"
          val future = CompletableFuture<String>()
          pending[id] = PendingAsk(id, sender, count, total, namesJSON, future)
          try {
            return future.get(35, TimeUnit.SECONDS)
          } catch (_: Exception) {
            return "timeout"
          } finally {
            pending.remove(id)
          }
        }
      })
      node = n
      return n
    }
  }

  private fun statusOf(n: Node?): JSObject {
    if (n == null) {
      return JSObject().put("state", "off")
    }
    return try {
      JSObject.fromJSONObject(org.json.JSONObject(n.status()))
    } catch (e: Exception) {
      JSObject().put("state", "error").put("error", e.message ?: "status failed")
    }
  }

  private fun nodeOrReject(invoke: Invoke): Node? {
    val n = node
    if (n == null) invoke.reject("内置节点未启动")
    return n
  }

  @Command
  fun status(invoke: Invoke) {
    invoke.resolve(statusOf(node))
  }

  @Command
  fun ensure(invoke: Invoke) {
    invoke.resolve(statusOf(ensureNode()))
  }

  @Command
  fun start_receiver(invoke: Invoke) {
    val n = nodeOrReject(invoke) ?: return
    val args = invoke.getArgs()
    val token = args.getString("token", "") ?: ""
    val confirm = args.getBoolean("confirm", false)
    try {
      n.startReceiver(token, confirm)
      invoke.resolve(JSObject().put("ok", true))
    } catch (e: Exception) {
      invoke.reject(e.message ?: "startReceiver failed")
    }
  }

  @Command
  fun stop_receiver(invoke: Invoke) {
    val n = nodeOrReject(invoke) ?: return
    try {
      n.stopReceiver()
      invoke.resolve(JSObject().put("ok", true))
    } catch (e: Exception) {
      invoke.reject(e.message ?: "stopReceiver failed")
    }
  }

  @Command
  fun pending(invoke: Invoke) {
    val asks = JSArray()
    for ((_, ask) in pending) {
      val o = JSObject()
        .put("id", ask.id)
        .put("sender", ask.sender)
        .put("count", ask.count)
        .put("total", ask.total)
      o.put("names", JSONArray(ask.names))
      asks.put(o)
    }
    invoke.resolve(JSObject().put("asks", asks))
  }

  @Command
  fun respond(invoke: Invoke) {
    val args = invoke.getArgs()
    val id = args.getString("id", "")
    val accept = args.getBoolean("accept", false)
    val ask = pending.remove(id)
    if (ask == null) {
      invoke.reject("no pending ask $id")
    } else {
      ask.future.complete(if (accept) "accept" else "denied")
      invoke.resolve(JSObject().put("ok", true))
    }
  }

  @Command
  fun ping_target(invoke: Invoke) {
    val n = nodeOrReject(invoke) ?: return
    val args = invoke.getArgs()
    try {
      val result = n.pingTarget(normalizeTarget(args.getString("target", "") ?: ""), args.getString("token", ""))
      invoke.resolve(JSObject.fromJSONObject(org.json.JSONObject(result)))
    } catch (e: Exception) {
      invoke.reject(e.message ?: "pingTarget failed")
    }
  }

  @Command
  fun send_files(invoke: Invoke) {
    val n = nodeOrReject(invoke) ?: return
    val args = invoke.getArgs()
    val files = try {
      JSArray(args.getString("paths", "[]"))
    } catch (_: Exception) {
      JSArray()
    }
    val target = normalizeTarget(args.getString("target", "") ?: "")
    val token = args.getString("token", "") ?: ""
    val sender = args.getString("sender", hostName()) ?: hostName()
    // Blocks until the batch finishes: run on a worker thread, never on the
    // thread PluginManager dispatched on.
    Thread {
      try {
        val result = n.sendFiles(target, token, sender, files.toString())
        val payload = JSObject.fromJSONObject(org.json.JSONObject(result))
        // resolve is thread-safe: it posts back through the JNI bridge.
        invoke.resolve(payload)
      } catch (e: Exception) {
        invoke.reject(e.message ?: "sendFiles failed")
      }
    }.start()
  }

  @Command
  fun open_login(invoke: Invoke) {
    val n = nodeOrReject(invoke) ?: return
    val url = try {
      org.json.JSONObject(n.status()).optString("login_url", "")
    } catch (_: Exception) {
      ""
    }
    if (url.isEmpty()) {
      invoke.reject("当前不需要登录")
    } else {
      try {
        activity.startActivity(android.content.Intent(android.content.Intent.ACTION_VIEW, android.net.Uri.parse(url)))
        invoke.resolve(JSObject().put("ok", true))
      } catch (e: Exception) {
        invoke.reject(e.message ?: "无法打开浏览器")
      }
    }
  }

  private fun hostName(): String {
    val n = node ?: return Build.MODEL ?: "tailsend"
    return try {
      org.json.JSONObject(n.status()).optString("hostname", Build.MODEL ?: "tailsend")
    } catch (_: Exception) {
      Build.MODEL ?: "tailsend"
    }
  }
}
