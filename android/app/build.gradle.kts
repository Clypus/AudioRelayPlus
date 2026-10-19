import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Release signing: env vars (CI) or a gitignored android/keystore.properties (local builds).
// Without either, the release APK stays unsigned on purpose — no fallback to the debug key.
val keystoreProps = Properties().apply {
    val f = rootProject.file("keystore.properties")
    if (f.isFile) f.inputStream().use { load(it) }
}

fun signingValue(env: String, prop: String): String? =
    System.getenv(env)?.takeIf { it.isNotBlank() }
        ?: keystoreProps.getProperty(prop)?.takeIf { it.isNotBlank() }

val releaseStoreFile = signingValue("ARP_KEYSTORE_FILE", "storeFile")

android {
    namespace = "com.audiorelayplus.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.audiorelayplus.app"
        minSdk = 26
        targetSdk = 34
        versionCode = 4
        versionName = "0.4.0"
    }

    signingConfigs {
        if (releaseStoreFile != null) {
            create("release") {
                // relative paths are resolved against android/
                storeFile = rootProject.file(releaseStoreFile)
                storePassword = signingValue("ARP_KEYSTORE_PASSWORD", "storePassword")
                keyAlias = signingValue("ARP_KEY_ALIAS", "keyAlias")
                keyPassword = signingValue("ARP_KEY_PASSWORD", "keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfigs.findByName("release")?.let { signingConfig = it }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    // Pure-Java Opus encoder (a port of libopus) — no NDK needed
    implementation("io.github.jaredmdobson:concentus:1.0.2")
}
