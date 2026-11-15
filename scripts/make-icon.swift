#!/usr/bin/env swift
// 사용법: swift scripts/make-icon.swift <appiconset 출력 디렉터리>
// 예:    swift scripts/make-icon.swift apple/Assets.xcassets/AppIcon.appiconset
// AppKit/CoreGraphics로 1024x1024 아이콘을 그리고 sips로 각 크기 PNG와
// Contents.json을 만든다. 출력 디렉터리 부모가 *.xcassets면 부모 Contents.json도 쓴다.
// 디자인: "Pier(부두)". 하늘→바다 파란 그라데이션, 청록 물결,
// 목재 부두 데크와 기둥, 끝의 앰버 등불.

import AppKit
import CoreGraphics
import Foundation

let canvas: CGFloat = 1024
let iconRect = CGRect(x: 100, y: 100, width: 824, height: 824)
let cornerRadius: CGFloat = 185

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("make-icon: \(message)\n".utf8))
    exit(1)
}

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
        blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
}

// MARK: - 그리기

/// 하늘 → 깊은 바다 세로 그라데이션.
func drawBackground(_ ctx: CGContext) {
    let gradient = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [color(0x4FA8FF), color(0x1F5FD1), color(0x0B2E6E)] as CFArray,
        locations: [0, 0.52, 1]
    )
    guard let gradient else { return }
    ctx.drawLinearGradient(
        gradient, start: CGPoint(x: 512, y: iconRect.maxY),
        end: CGPoint(x: 512, y: iconRect.minY), options: [])
}

/// 사인 곡선 물결 한 줄. 데크가 위에 그려지므로 좌우를 가르지 않는다.
/// 둥근 사각형 바깥까지 그려 클립으로 양 끝이 자연스럽게 잘리게 하고,
/// 첫 점도 곡선 위에 놓아 시작부가 꺾이지 않게 한다.
func drawWave(_ ctx: CGContext, baseY: CGFloat, amplitude: CGFloat, phase: CGFloat, alpha: CGFloat) {
    let path = CGMutablePath()
    var first = true
    for x in stride(from: 40, through: 984, by: 10) {
        let point = CGPoint(x: CGFloat(x), y: baseY + amplitude * sin((CGFloat(x) + phase) / 55))
        if first {
            path.move(to: point)
            first = false
        } else {
            path.addLine(to: point)
        }
    }
    ctx.addPath(path)
    ctx.setStrokeColor(color(0x9FE8DD, alpha))
    ctx.setLineWidth(13)
    ctx.setLineCap(.round)
    ctx.strokePath()
}

/// 원근감 있는 부두 데크. 먼 끝(수평선 쪽)은 좁고 가까운 쪽은 넓다.
/// 반환값은 기둥/등불이 쓰는 (먼 끝 y, 가까운 끝 y, 중심 x).
func drawDeck(_ ctx: CGContext) -> (farY: CGFloat, nearY: CGFloat, cx: CGFloat) {
    let cx: CGFloat = 512
    let farY: CGFloat = 560
    let nearY: CGFloat = 112
    let farHalf: CGFloat = 46
    let nearHalf: CGFloat = 225

    func lerp(_ a: CGFloat, _ b: CGFloat, _ t: CGFloat) -> CGFloat { a + (b - a) * t }
    func yAt(_ t: CGFloat) -> CGFloat { lerp(farY, nearY, t) }
    func halfAt(_ t: CGFloat) -> CGFloat { lerp(farHalf, nearHalf, t) }

    // 판자 면
    let deck = CGMutablePath()
    deck.move(to: CGPoint(x: cx - farHalf, y: farY))
    deck.addLine(to: CGPoint(x: cx + farHalf, y: farY))
    deck.addLine(to: CGPoint(x: cx + nearHalf, y: nearY))
    deck.addLine(to: CGPoint(x: cx - nearHalf, y: nearY))
    deck.closeSubpath()
    ctx.addPath(deck)
    let wood = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [color(0xEFE0C4), color(0xDDBB8D)] as CFArray,
        locations: [0, 1]
    )
    ctx.saveGState()
    ctx.clip()
    if let wood {
        ctx.drawLinearGradient(
            wood, start: CGPoint(x: cx, y: farY), end: CGPoint(x: cx, y: nearY), options: [])
    }
    ctx.restoreGState()

    // 가장자리 빔(옆면 느낌의 어두운 테두리)
    ctx.addPath(deck)
    ctx.setStrokeColor(color(0xA97F50))
    ctx.setLineWidth(9)
    ctx.setLineJoin(.round)
    ctx.strokePath()

    // 판자 이음새. 가까울수록 간격이 넓어지는 원근 배치.
    ctx.setStrokeColor(color(0x8A6030, 0.55))
    ctx.setLineWidth(7)
    ctx.setLineCap(.round)
    for t in [0.16, 0.32, 0.5, 0.7] as [CGFloat] {
        let y = yAt(t)
        let half = halfAt(t) - 6
        ctx.move(to: CGPoint(x: cx - half, y: y))
        ctx.addLine(to: CGPoint(x: cx + half, y: y))
        ctx.strokePath()
    }

    // 데크 중앙 길이 방향 줄
    ctx.setStrokeColor(color(0x8A6030, 0.35))
    ctx.setLineWidth(6)
    ctx.move(to: CGPoint(x: cx, y: farY - 4))
    ctx.addLine(to: CGPoint(x: cx, y: nearY + 6))
    ctx.strokePath()

    // 기둥 두 쌍. 가까운 기둥이 더 굵고 길다.
    for t in [0.38, 0.75] as [CGFloat] {
        let y = yAt(t)
        let half = halfAt(t)
        let w = 16 + 14 * t
        let above = 30 + 28 * t
        let below = 55 + 60 * t
        for side in [-1, 1] as [CGFloat] {
            let x = cx + side * (half + w * 0.5 - 3)
            let rect = CGRect(x: x - w / 2, y: y - below, width: w, height: above + below)
            ctx.setFillColor(color(0x6B4E33))
            ctx.addPath(CGPath(roundedRect: rect, cornerWidth: 5, cornerHeight: 5, transform: nil))
            ctx.fillPath()
            // 기둥 머리 위쪽 캡
            let cap = CGRect(x: x - w / 2 - 2, y: y + above - 8, width: w + 4, height: 8)
            ctx.setFillColor(color(0x8A6844))
            ctx.addPath(CGPath(roundedRect: cap, cornerWidth: 3, cornerHeight: 3, transform: nil))
            ctx.fillPath()
        }
    }
    return (farY, nearY, cx)
}

/// 데크 끝의 앰버 등불과 주변 빛.
func drawLantern(_ ctx: CGContext, baseY: CGFloat, cx: CGFloat) {
    // 기둥
    ctx.setFillColor(color(0x5C422C))
    ctx.addPath(
        CGPath(
            roundedRect: CGRect(x: cx - 6, y: baseY, width: 12, height: 80),
            cornerWidth: 4, cornerHeight: 4, transform: nil))
    ctx.fillPath()

    // 빛 번짐(라디얼 그라데이션)
    let globeY = baseY + 96
    let glow = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [color(0xFFC15C, 0.6), color(0xFFC15C, 0)] as CFArray,
        locations: [0, 1]
    )
    if let glow {
        ctx.drawRadialGradient(
            glow, startCenter: CGPoint(x: cx, y: globeY), startRadius: 4,
            endCenter: CGPoint(x: cx, y: globeY), endRadius: 62, options: [])
    }

    // 등불 지붕
    let roof = CGMutablePath()
    roof.move(to: CGPoint(x: cx - 24, y: globeY + 16))
    roof.addLine(to: CGPoint(x: cx + 24, y: globeY + 16))
    roof.addLine(to: CGPoint(x: cx, y: globeY + 34))
    roof.closeSubpath()
    ctx.setFillColor(color(0x5C422C))
    ctx.addPath(roof)
    ctx.fillPath()

    // 등불 유리(앰버)
    ctx.setFillColor(color(0xFFC15C))
    ctx.addEllipse(in: CGRect(x: cx - 19, y: globeY - 19, width: 38, height: 38))
    ctx.fillPath()
}

/// 1024x1024 원본을 그려 PNG 바이트로 반환한다.
func renderPNG() -> Data {
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: Int(canvas),
        pixelsHigh: Int(canvas),
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0,
        bitsPerPixel: 0
    ) else { fail("비트맵 생성 실패") }

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    guard let ctx = NSGraphicsContext.current?.cgContext else { fail("그래픽 컨텍스트 없음") }

    ctx.interpolationQuality = .high
    ctx.setShouldAntialias(true)

    let body = CGPath(
        roundedRect: iconRect, cornerWidth: cornerRadius,
        cornerHeight: cornerRadius, transform: nil)
    // 아이콘 몸통 아래쪽에 은은한 그림자를 깐다.
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 30, color: color(0x000000, 0.35))
    ctx.setFillColor(color(0x0B2E6E))
    ctx.addPath(body)
    ctx.fillPath()
    ctx.restoreGState()
    ctx.addPath(body)
    ctx.clip()
    drawBackground(ctx)
    drawWave(ctx, baseY: 470, amplitude: 14, phase: 0, alpha: 0.5)
    drawWave(ctx, baseY: 385, amplitude: 16, phase: 90, alpha: 0.4)
    drawWave(ctx, baseY: 300, amplitude: 17, phase: 200, alpha: 0.3)
    let deck = drawDeck(ctx)
    drawLantern(ctx, baseY: deck.farY, cx: deck.cx)

    NSGraphicsContext.restoreGraphicsState()
    guard let png = rep.representation(using: .png, properties: [:]) else { fail("PNG 인코딩 실패") }
    return png
}

func run(_ launchPath: String, _ args: [String]) throws {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: launchPath)
    process.arguments = args
    try process.run()
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        fail("sips 실패(exit \(process.terminationStatus)): \(args.joined(separator: " "))")
    }
}

/// macOS 앱 아이콘 슬롯 10칸(16/32/128/256/512pt의 1x·2x)에 맞는 Contents.json.
func writeContentsJSON(to dir: URL) throws {
    var images: [[String: String]] = []
    for points in [16, 32, 128, 256, 512] {
        for (scale, pixels) in [("1x", points), ("2x", points * 2)] {
            images.append([
                "filename": "icon_\(pixels).png",
                "idiom": "mac",
                "scale": scale,
                "size": "\(points)x\(points)",
            ])
        }
    }
    let json: [String: Any] = [
        "images": images,
        "info": ["author": "xcode", "version": 1],
    ]
    let data = try JSONSerialization.data(
        withJSONObject: json, options: [.prettyPrinted, .sortedKeys])
    try data.write(to: dir.appendingPathComponent("Contents.json"))
}

/// *.xcassets 루트의 최소 Contents.json(없을 때만 만든다).
func writeCatalogJSON(at dir: URL) throws {
    let file = dir.appendingPathComponent("Contents.json")
    guard !FileManager.default.fileExists(atPath: file.path) else { return }
    let json: [String: Any] = ["info": ["author": "xcode", "version": 1]]
    let data = try JSONSerialization.data(
        withJSONObject: json, options: [.prettyPrinted, .sortedKeys])
    try data.write(to: file)
}

guard CommandLine.arguments.count == 2 else {
    fail("사용법: swift scripts/make-icon.swift <appiconset 출력 디렉터리>")
}
let outDir = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)

let master = outDir.appendingPathComponent("icon_1024.png")
try renderPNG().write(to: master)

for size in [16, 32, 64, 128, 256, 512] {
    let out = outDir.appendingPathComponent("icon_\(size).png")
    try run("/usr/bin/sips", ["-z", "\(size)", "\(size)", master.path, "--out", out.path])
}

try writeContentsJSON(to: outDir)
if outDir.pathExtension == "appiconset" {
    let parent = outDir.deletingLastPathComponent()
    if parent.pathExtension == "xcassets" {
        try writeCatalogJSON(at: parent)
    }
}

print("아이콘 생성 완료: \(outDir.path)")
