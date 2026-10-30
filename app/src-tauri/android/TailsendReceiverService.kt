package io.tailsend.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

// Keeps the process alive while Tailsend's receiver listens, so transfers
// survive the app being backgrounded or the screen locking. OEM task managers
// (Huawei/HONOR in particular) are far less aggressive with a process that
// hosts a foreground service.
class TailsendReceiverService : Service() {

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    startForegroundCompat()
    return START_STICKY
  }

  private fun startForegroundCompat() {
    val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val ch = NotificationChannel(
        CHANNEL_ID,
        "接收端运行中",
        NotificationManager.IMPORTANCE_LOW, // silent, no heads-up
      )
      ch.description = "保持 Tailsend 接收端在后台运行，随时接收文件"
      nm.createNotificationChannel(ch)
    }
    val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      Notification.Builder(this, CHANNEL_ID)
    } else {
      @Suppress("DEPRECATION")
      Notification.Builder(this)
    }
    val notif = builder
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(getString(R.string.app_name))
      .setContentText("接收端运行中，可随时接收文件")
      .setOngoing(true)
      .build()
    // The specialUse type is enforced (and only known) on API 34+; older
    // systems take the plain two-arg call.
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
    } else {
      startForeground(NOTIF_ID, notif)
    }
  }

  companion object {
    private const val CHANNEL_ID = "tailsend_receiver"
    private const val NOTIF_ID = 4782

    // Called from MainActivity.onCreate — starting while the app is in the
    // foreground sidesteps Android 12+ background-start restrictions.
    fun start(ctx: Context) {
      val i = Intent(ctx, TailsendReceiverService::class.java)
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        ctx.startForegroundService(i)
      } else {
        ctx.startService(i)
      }
    }
  }
}
