import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.tailsend.plugin.tsnet"
    compileSdk = 37

    defaultConfig {
        minSdk = 24
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

dependencies {
    implementation(project(":tauri-android"))
    // FileProvider (tap-to-open received files / install received APKs).
    // tauri-android keeps androidx.core as `implementation`, so it does not
    // reach this module's compile classpath — declare it; version matches
    // tauri-android's core-ktx so the artifact is already in the graph.
    implementation("androidx.core:core-ktx:1.7.0")
    // tailsend's embedded node (gomobile bind of tailsend/mobile); generated
    // by apply-node.sh / release.yml — never committed.
    implementation(files("libs/tailsend-mobile.aar"))
}
