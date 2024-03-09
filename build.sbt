ThisBuild / version := "0.1.0"

ThisBuild / scalaVersion := "3.3.1"

lazy val root = (project in file("."))
    .enablePlugins(GraalVMNativeImagePlugin)
    .settings(
        name := "liv",
        description := "Liv (named after historian Titus Livius) is a CLI tool to backup GitHub data",
        libraryDependencies += "org.scala-lang" %% "toolkit"  % "0.2.1",
        libraryDependencies += "com.47deg"      %% "github4s" % "0.33.3",
        libraryDependencies += "org.ekrich"     %% "sconfig"  % "1.6.0",
        libraryDependencies += "org.eclipse.jgit" % "org.eclipse.jgit" % "6.8.0.202311291450-r",
        graalVMNativeImageOptions ++= Seq(
            "--no-fallback",
            "--enable-url-protocols=https",
            "--gc=G1",
            "-Djava.net.preferIPv6Addresses=true",
            "-H:Optimize=2"
        )
    )
