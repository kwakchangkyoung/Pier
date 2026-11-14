import Foundation
import Security

/// 번들 ID 변경(com.yourname.pier → com.gajago.pier) 전 설치본의 데이터를 새 위치로 이관한다.
/// 앱 시작 시 한 번만 실행되며, 완료 여부는 새 Application Support 안의 마커 파일로 기억한다.
/// - `connections.json`: 옛 위치에서 새 위치로 복사한다(원본은 남긴다).
/// - 키체인: 각 연결 id의 시크릿을 옛 service에서 새 service로 복사한다(옛 항목은 남긴다).
/// - Caches: 재생성 가능하므로 옮기지 않는다.
enum LegacyMigration {
    /// 옛 번들 ID. 이 파일에서만 참조한다.
    static let legacyBundleId = "com.yourname.pier"

    /// 이관 완료를 표시하는 마커 파일 이름 (새 Application Support 디렉터리 안).
    private static let markerFileName = ".legacy-migration-done"

    /// 이관에 쓰는 파일 위치 묶음.
    struct Paths {
        var legacyConnectionsFile: URL
        var newConnectionsFile: URL
        var markerFile: URL
    }

    /// 현재 사용자 홈 기준의 이관 경로를 계산한다.
    static func resolvePaths(fileManager: FileManager = .default) -> Paths {
        let supportRoot =
            fileManager
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return Paths(
            legacyConnectionsFile:
                supportRoot
                .appendingPathComponent(legacyBundleId, isDirectory: true)
                .appendingPathComponent("connections.json"),
            newConnectionsFile: PierPaths.connectionsFileURL,
            markerFile: PierPaths.applicationSupportDirectory
                .appendingPathComponent(markerFileName)
        )
    }

    /// 옛 connections.json을 새 위치로 복사할지 판단한다.
    /// 옛 파일이 있고 새 파일이 없을 때만 복사한다 — 이미 있는 새 파일은 절대 덮어쓰지 않는다.
    static func shouldCopyConnections(legacyExists: Bool, newExists: Bool) -> Bool {
        legacyExists && !newExists
    }

    /// 앱 시작 시 한 번 호출한다. ConnectionStore가 connections.json을 읽기 전에 끝나야 한다.
    /// 이관이 이미 끝났거나 옛 데이터가 없으면 바로 반환한다.
    /// 옛 Application Support 디렉터리와 옛 service 키체인 항목은 일부러 지우지 않는다.
    /// 사용자가 이전 빌드로 돌아갈 수 있게 남겨 두며, 정리하려면
    /// `~/Library/Application Support/com.yourname.pier`를 지우고 키체인 접근 앱에서
    /// service가 "com.yourname.pier"인 항목을 지우면 된다.
    static func runIfNeeded(fileManager: FileManager = .default) {
        let paths = resolvePaths(fileManager: fileManager)
        if fileManager.fileExists(atPath: paths.markerFile.path) {
            return
        }

        let legacyExists = fileManager.fileExists(atPath: paths.legacyConnectionsFile.path)
        let newExists = fileManager.fileExists(atPath: paths.newConnectionsFile.path)
        if !legacyExists {
            // 옛 설치 흔적이 없으면 할 일이 없다. 마커만 남긴다.
            writeMarker(paths: paths, fileManager: fileManager)
            return
        }

        if shouldCopyConnections(legacyExists: legacyExists, newExists: newExists) {
            do {
                try fileManager.createDirectory(
                    at: paths.newConnectionsFile.deletingLastPathComponent(),
                    withIntermediateDirectories: true
                )
                try fileManager.copyItem(at: paths.legacyConnectionsFile, to: paths.newConnectionsFile)
                Log.app.info("레거시 connections.json을 새 위치로 복사했다")
            } catch {
                Log.app.error("레거시 connections.json 복사 실패: \(error.localizedDescription)")
                // 다음 실행 때 다시 시도하도록 마커를 남기지 않는다.
                return
            }
        }

        migrateKeychainItems(fileManager: fileManager, paths: paths)
        writeMarker(paths: paths, fileManager: fileManager)
    }

    /// 각 연결 id의 시크릿을 옛 키체인 service에서 새 service로 복사한다.
    /// 한 연결이 실패해도 나머지는 계속하며, 시크릿 값은 로그에 남기지 않는다.
    private static func migrateKeychainItems(fileManager: FileManager, paths: Paths) {
        guard let data = fileManager.contents(atPath: paths.newConnectionsFile.path),
            let connections = try? JSONDecoder().decode([SavedConnection].self, from: data)
        else {
            return
        }
        for connection in connections {
            let account = connection.id.uuidString
            do {
                // 새 service에 이미 항목이 있으면 건드리지 않는다.
                if try Keychain.load(account: account) != nil {
                    continue
                }
                guard let secret = try loadLegacySecret(account: account) else {
                    continue
                }
                try Keychain.save(secret, account: account)
            } catch {
                Log.app.error("레거시 키체인 이관 실패 — 연결 1건 건너뜀")
            }
        }
    }

    /// 옛 번들의 키체인 service에서 시크릿을 읽는다. 항목이 없으면 nil.
    /// 접근 거부·취소 등의 실패는 오류로 던져 호출자가 해당 연결만 건너뛰게 한다.
    private static func loadLegacySecret(account: String) throws -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: legacyBundleId,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess, let data = result as? Data,
            let secret = String(data: data, encoding: .utf8)
        else {
            throw Keychain.KeychainError.failure(operation: "레거시 읽기", status: status)
        }
        return secret
    }

    /// 이관 완료 마커를 쓴다. 실패하면 다음 실행 때 다시 시도할 뿐 치명적이지 않다.
    private static func writeMarker(paths: Paths, fileManager: FileManager) {
        do {
            try fileManager.createDirectory(
                at: paths.markerFile.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            try Data().write(to: paths.markerFile)
        } catch {
            Log.app.error("레거시 이관 마커 쓰기 실패: \(error.localizedDescription)")
        }
    }
}
