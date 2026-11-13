import Foundation

/// BrowserModel의 쓰기 작업(새 폴더/이름 변경/이동/삭제).
/// 판단은 코어가 하고 여기서는 호출만 한다. 목록 반영은 applyWriteResults가 담당한다.
extension BrowserModel {
    /// 업로드 완료 등으로 원격이 바뀌었을 때 호출한다.
    /// entry가 현재 폴더 것이면 즉시 upsert하고 백그라운드 refresh를 시작한다.
    func noteRemoteChange(entry: PierEntry? = nil) {
        applyWriteResults(upserted: entry.map { [$0] } ?? [])
    }

    /// 현재 폴더에 새 폴더를 만든다. 이름은 trim 후 비어 있지 않고 '/'가 없어야 한다.
    @discardableResult
    func createFolder(named name: String) async throws -> PierEntry {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !trimmed.contains("/") else {
            throw PierSessionError.invalidName
        }
        guard let conn = connection else { throw PierSessionError.unavailable }
        let entry = try await conn.createFolder(parent: currentFolder, name: trimmed)
        applyWriteResults(upserted: [entry])
        return entry
    }

    /// 이름 변경과 이동 공통 API. newParent는 '/'로 끝나는 폴더 키(루트는 "").
    /// 대상 이름이 이미 있으면 코어가 Conflict를 던진다.
    @discardableResult
    func renameEntry(key: String, newParent: String, newName: String) async throws -> PierEntry {
        guard let conn = connection else { throw PierSessionError.unavailable }
        let entry = try await conn.rename(key: key, newParent: newParent, newName: newName)
        applyWriteResults(removed: [key], upserted: [entry])
        return entry
    }

    /// 앱 내부 이동. 원래 이름을 유지한 채 newParent 폴더로 rename한다.
    /// 같은 폴더로의 이동과 폴더를 자기 자신/하위로 옮기는 경우는 건너뛴다.
    /// 항목별 오류는 첫 번째 것을 모아 마지막에 던진다(나머지는 계속 진행).
    func moveEntries(keys: [String], toParent newParent: String) async throws {
        guard let conn = connection else { throw PierSessionError.unavailable }
        var removed: [String] = []
        var moved: [PierEntry] = []
        var firstError: Error?
        for key in keys {
            guard RemoteKey.parent(of: key) != newParent else { continue }
            // 폴더 키("a/b/")를 자기 자신이나 하위("a/b/…")로는 옮길 수 없다.
            if key.hasSuffix("/"), newParent == key || newParent.hasPrefix(key) {
                continue
            }
            do {
                let entry = try await conn.rename(
                    key: key,
                    newParent: newParent,
                    newName: RemoteKey.name(of: key)
                )
                removed.append(key)
                moved.append(entry)
            } catch {
                firstError = firstError ?? error
            }
        }
        applyWriteResults(removed: removed, upserted: moved)
        if let firstError { throw firstError }
    }

    /// 선택 항목을 삭제한다. 폴더 키는 하위 객체 전체가 삭제된다(코어 처리).
    func deleteEntries(keys: [String]) async throws {
        guard let conn = connection else { throw PierSessionError.unavailable }
        for key in keys {
            try await conn.delete(key: key)
        }
        applyWriteResults(removed: keys)
    }
}
