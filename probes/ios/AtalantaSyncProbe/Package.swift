// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "AtalantaSyncProbe",
    platforms: [.iOS(.v14)],
    products: [
        .library(
            name: "AtalantaSyncProbe",
            targets: ["AtalantaSyncProbe"]
        ),
    ],
    targets: [
        .target(
            name: "AtalantaSyncProbe",
            path: "Sources"
        ),
    ]
)
