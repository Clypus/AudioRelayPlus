plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.audiorelayplus.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.audiorelayplus.app"
        minSdk = 26
        targetSdk = 34
        versionCode = 2
        versionName = "0.1.1"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
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
    // Saf Java Opus kodlayıcı (libopus portu) — NDK gerektirmez
    implementation("io.github.jaredmdobson:concentus:1.0.2")
}
