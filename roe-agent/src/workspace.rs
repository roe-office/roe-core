//! Agent 专属文件工作区（沙盒）。
//!
//! 设计依据：ROE_Agent_Workspace_设计文档_v1.md，三、3.2-3.5。
//!
//! 核心约束（写代码时必须遵守，不能图省事绕过）：
//! workspace 内部的所有文件读写，必须经过 WorkspaceContext 持有的
//! cap_std::fs::Dir 实例的方法，不能直接调用 std::fs::* 系列函数
//! 操作 workspace 内的路径。这是本设计的安全保证的唯一来源——
//! cap-std 在操作系统层面阻止路径穿越和 symlink 逃逸，一旦绕过
//! Dir 直接用 std::fs，这个保证就形同虚设。
//!
//! 目录结构（v1 设计文档 三、3.2）：
//!   <workspace_root>/
//!   ├── media/     媒体资产（图片等），按内容哈希命名（CAS，见 ingest 模块）
//!   ├── exports/   AI 产物（保留原文件名，重名自动加序号）
//!   └── inputs/    用户上传的文档（保留原文件名）

use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;

/// media/ 子目录名，虚拟路径的第一段固定是这个。
pub const MEDIA_DIR: &str = "media";
/// exports/ 子目录名：AI 产物。
pub const EXPORTS_DIR: &str = "exports";
/// inputs/ 子目录名：用户上传的文档。
pub const INPUTS_DIR: &str = "inputs";

/// Agent 工作区上下文：持有一个 cap-std 沙盒化的目录句柄。
///
/// 会话启动时构造一次，通过 Arc 注入进各个需要文件访问能力的 Tool，
/// 全程不变（不像 AgentContextProvider 那样是运行时可变的
/// Arc<RwLock<Option<T>>>——workspace_root 在一次会话期间是固定的
/// 环境配置，见设计文档 三、3.4）。
pub struct WorkspaceContext {
    root: Dir,
    /// 宿主机物理根路径（规范化）。只用于展示和"打开所在文件夹"，不用于文件 IO。
    physical_root: PathBuf,
}

/// 工作区内一个文件的条目（list 的返回项）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceEntry {
    /// 虚拟路径，形如 "/exports/a.xlsx"。
    pub virtual_path: String,
    pub name: String,
    pub size_bytes: u64,
}

/// WorkspaceContext 构造/操作失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("failed to create workspace root directory {path}: {source}")]
    CreateRootFailed {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to open workspace root directory {path} with cap-std: {source}")]
    OpenRootFailed {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to create subdirectory {name} inside workspace: {source}")]
    CreateSubdirFailed {
        name: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to write file {virtual_path} inside workspace: {source}")]
    WriteFailed {
        virtual_path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to read file {virtual_path} inside workspace: {source}")]
    ReadFailed {
        virtual_path: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to list directory {dir} inside workspace: {source}")]
    ListFailed {
        dir: String,
        #[source]
        source: io::Error,
    },
    #[error("invalid file name {0:?}: must be a plain file name without path separators")]
    InvalidFileName(String),
    #[error("directory {0:?} is not allowed here (writable: \"inputs\" / \"exports\"; listable: also \"media\")")]
    InvalidDirectory(String),
    #[error("path {0:?} does not exist in workspace")]
    PathNotFound(String),
    #[error("too many files with a name like {0:?} in the same directory")]
    TooManyDuplicates(String),
    /// 虚拟路径不符合约定格式（见设计文档 三、3.1：必须以 "/" 开头）。
    #[error("invalid virtual path {0:?}: must start with '/', e.g. \"/media/xxx.png\"")]
    InvalidVirtualPath(String),
}

impl WorkspaceContext {
    /// 打开（或首次创建）workspace 根目录，并确保 media/、exports/
    /// 两个子目录存在。
    ///
    /// `physical_root` 是宿主机上的真实路径（比如 main.rs 里从命令行
    /// 参数或默认值算出来的那个 PathBuf）。这是本模块里唯一一次触碰
    /// "ambient authority"（不受限的系统级文件访问）——之后所有操作
    /// 都通过返回的 Dir 句柄进行，不再需要、也不应该再直接碰物理路径。
    pub fn open_or_create(physical_root: &Path) -> Result<Self, WorkspaceError> {
        // 用普通 std::fs 创建根目录本身：这一步发生在 cap-std 沙盒
        // "之外"，是无法避免的——总得有一次操作先把这个目录本身
        // 造出来，cap-std 才能"进去"。根目录创建之后，本函数返回的
        // Self 内部就再也不会出现 std::fs 调用了。
        std::fs::create_dir_all(physical_root).map_err(|source| {
            WorkspaceError::CreateRootFailed {
                path: physical_root.display().to_string(),
                source,
            }
        })?;

        let root = Dir::open_ambient_dir(physical_root, ambient_authority()).map_err(|source| {
            WorkspaceError::OpenRootFailed {
                path: physical_root.display().to_string(),
                source,
            }
        })?;

        // media/、exports/ 两个子目录：用 Dir::create_dir，已存在时
        // 忽略 AlreadyExists 错误（幂等：重复调用 open_or_create 不
        // 应该报错）。
        for name in [MEDIA_DIR, EXPORTS_DIR, INPUTS_DIR] {
            match root.create_dir(name) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(WorkspaceError::CreateSubdirFailed { name, source });
                }
            }
        }

        // 仅用于展示/打开文件夹的路径规范化，不参与任何文件 IO。
        let physical_root =
            std::fs::canonicalize(physical_root).unwrap_or_else(|_| physical_root.to_path_buf());

        Ok(Self { root, physical_root })
    }

    /// 把虚拟路径（形如 "/media/xxx.png"）解析成"相对于 workspace
    /// 根目录"的相对路径字符串，供后续 Dir 方法使用。
    ///
    /// 只做格式校验（必须以 '/' 开头），真正的路径穿越防护由 cap-std
    /// 的 Dir 在实际打开/创建文件时于操作系统层面保证，这里不需要、
    /// 也不应该重复手写 ".." 检测之类的逻辑（那正是设计文档里明确
    /// 放弃的"用户态字符串比对"方案）。
    fn strip_virtual_prefix(virtual_path: &str) -> Result<&str, WorkspaceError> {
        virtual_path
            .strip_prefix('/')
            .filter(|rest| !rest.is_empty())
            .ok_or_else(|| WorkspaceError::InvalidVirtualPath(virtual_path.to_string()))
    }

    /// 把字节内容写入 media/ 目录下指定文件名（不含目录前缀，调用方
    /// 传入类似 "a1b2c3....png" 这样已经算好 CAS 名字的文件名）。
    /// 若目标文件已存在，直接视为成功（CAS 去重语义：内容相同、
    /// 名字相同，不需要重写）。
    ///
    /// 返回写入后的虚拟路径（形如 "/media/xxx.png"）。
    pub fn write_media(&self, file_name: &str, bytes: &[u8]) -> Result<String, WorkspaceError> {
        let virtual_path = format!("/{MEDIA_DIR}/{file_name}");

        // CAS 去重：文件已存在则跳过写入。用 Dir::metadata 判断存在性，
        // 而不是 std::fs——同样是为了不绕过 cap-std 的沙盒边界。
        let already_exists = self
            .root
            .metadata(format!("{MEDIA_DIR}/{file_name}"))
            .is_ok();

        if !already_exists {
            self.root
                .write(format!("{MEDIA_DIR}/{file_name}"), bytes)
                .map_err(|source| WorkspaceError::WriteFailed {
                    virtual_path: virtual_path.clone(),
                    source,
                })?;
        }

        Ok(virtual_path)
    }

    /// 物理根目录（规范化绝对路径）。只用于展示和打开文件夹。
    pub fn physical_root(&self) -> &Path {
        &self.physical_root
    }

    /// 虚拟路径 -> 宿主机物理路径，仅用于展示/打开文件夹。
    /// 先让 cap-std 确认该路径在沙盒内且存在，再拼接，所以 ".." 越界会失败。
    pub fn physical_path(&self, virtual_path: &str) -> Result<PathBuf, WorkspaceError> {
        let relative = Self::strip_virtual_prefix(virtual_path)?;
        self.root
            .metadata(relative)
            .map_err(|_| WorkspaceError::PathNotFound(virtual_path.to_string()))?;
        Ok(self.physical_root.join(relative))
    }

    fn check_file_name(name: &str) -> Result<(), WorkspaceError> {
        let bad = name.is_empty()
            || name == "."
            || name == ".."
            || name.len() > 200
            || name.chars().any(|c| c == '/' || c == '\\' || c.is_control());
        if bad {
            Err(WorkspaceError::InvalidFileName(name.to_string()))
        } else {
            Ok(())
        }
    }

    /// 把字节写入 inputs/ 或 exports/ 下的指定文件名（保留原名）。
    /// 重名时自动加序号："a.xlsx" -> "a (1).xlsx"；用 create_new 打开，绝不覆盖已有文件。
    /// 返回实际写入的虚拟路径。
    pub fn write_file(
        &self,
        dir: &str,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, WorkspaceError> {
        if dir != INPUTS_DIR && dir != EXPORTS_DIR {
            return Err(WorkspaceError::InvalidDirectory(dir.to_string()));
        }
        Self::check_file_name(file_name)?;
        let (stem, ext) = match file_name.rfind('.') {
            Some(i) if i > 0 => (&file_name[..i], &file_name[i..]),
            _ => (file_name, ""),
        };
        for n in 0..1000u32 {
            let candidate = if n == 0 {
                file_name.to_string()
            } else {
                format!("{stem} ({n}){ext}")
            };
            let rel = format!("{dir}/{candidate}");
            let mut opts = cap_std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            match self.root.open_with(&rel, &opts) {
                Ok(mut f) => {
                    let virtual_path = format!("/{rel}");
                    f.write_all(bytes).map_err(|source| WorkspaceError::WriteFailed {
                        virtual_path: virtual_path.clone(),
                        source,
                    })?;
                    return Ok(virtual_path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(WorkspaceError::WriteFailed {
                        virtual_path: format!("/{rel}"),
                        source,
                    })
                }
            }
        }
        Err(WorkspaceError::TooManyDuplicates(file_name.to_string()))
    }

    /// 列出 media/、inputs/ 或 exports/ 下的文件（不含子目录），按文件名排序。
    pub fn list(&self, dir: &str) -> Result<Vec<WorkspaceEntry>, WorkspaceError> {
        if ![MEDIA_DIR, INPUTS_DIR, EXPORTS_DIR].contains(&dir) {
            return Err(WorkspaceError::InvalidDirectory(dir.to_string()));
        }
        let list_err = |source: io::Error| WorkspaceError::ListFailed {
            dir: dir.to_string(),
            source,
        };
        let mut out = Vec::new();
        for entry in self.root.read_dir(dir).map_err(list_err)? {
            let entry = entry.map_err(list_err)?;
            let meta = entry.metadata().map_err(list_err)?;
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // 办公软件的锁文件(.~lock.xxx#)不是用户内容,不列出
            if name.starts_with(".~") {
                continue;
            }
            out.push(WorkspaceEntry {
                virtual_path: format!("/{dir}/{name}"),
                name,
                size_bytes: meta.len(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// 根据虚拟路径读取 workspace 内的文件内容。
    ///
    /// 这是给 set_slide_background 的 picture 分支准备的读取入口
    /// （设计文档 五、3：那一步是 roe-script 侧的事，本次不实现，
    /// 但这个读取方法本次就位，供下次直接调用）。
    pub fn read(&self, virtual_path: &str) -> Result<Vec<u8>, WorkspaceError> {
        let relative = Self::strip_virtual_prefix(virtual_path)?;
        self.root
            .read(relative)
            .map_err(|source| WorkspaceError::ReadFailed {
                virtual_path: virtual_path.to_string(),
                source,
            })
    }
}

/// 桥接 roe-script 的 media_context::MediaAssetSource trait。
/// roe-script 不能依赖 roe-agent（依赖方向约束，见本文件顶部文档
/// 及 ROE_Agent_Workspace_设计文档_v1.md），所以 trait 定义在
/// roe-script 里，WorkspaceContext 在这里实现它；roe-agent::tool
/// 把 &WorkspaceContext 转成 &dyn MediaAssetSource 往下传给
/// execute_script_impl。
impl roe_script::media_context::MediaAssetSource for WorkspaceContext {
    fn read_media(&self, virtual_path: &str) -> Result<Vec<u8>, String> {
        self.read(virtual_path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立临时目录，避免互相干扰；用 tempfile 而非手写
    /// /tmp 路径拼接，防止并行测试之间撞名。
    fn temp_workspace() -> (tempfile::TempDir, WorkspaceContext) {
        let dir = tempfile::tempdir().expect("create temp dir for test");
        let ctx = WorkspaceContext::open_or_create(dir.path()).expect("open_or_create should succeed");
        (dir, ctx)
    }

    #[test]
    fn open_or_create_creates_media_and_exports_subdirs() {
        let (dir, _ctx) = temp_workspace();
        assert!(dir.path().join(MEDIA_DIR).is_dir());
        assert!(dir.path().join(EXPORTS_DIR).is_dir());
    }

    #[test]
    fn open_or_create_is_idempotent() {
        let dir = tempfile::tempdir().expect("create temp dir");
        WorkspaceContext::open_or_create(dir.path()).expect("first open should succeed");
        // 第二次对同一个物理目录调用，子目录已存在，不应该报错。
        WorkspaceContext::open_or_create(dir.path()).expect("second open should also succeed");
    }

    #[test]
    fn write_media_then_read_roundtrip() {
        let (_dir, ctx) = temp_workspace();
        let virtual_path = ctx
            .write_media("deadbeef.png", b"fake png bytes")
            .expect("write should succeed");
        assert_eq!(virtual_path, "/media/deadbeef.png");

        let content = ctx.read(&virtual_path).expect("read should succeed");
        assert_eq!(content, b"fake png bytes");
    }

    #[test]
    fn write_media_deduplicates_existing_file() {
        let (_dir, ctx) = temp_workspace();
        let first = ctx
            .write_media("samehash.png", b"content A")
            .expect("first write should succeed");
        // 同名文件第二次写入：CAS 语义下应视为已存在、直接复用，
        // 不应该报错，也不应该用第二份内容覆盖第一份。
        let second = ctx
            .write_media("samehash.png", b"content B (should be ignored)")
            .expect("second write should succeed (dedup, not overwrite)");
        assert_eq!(first, second);

        let content = ctx.read(&first).expect("read should succeed");
        assert_eq!(
            content, b"content A",
            "dedup must keep the original content, not overwrite with the second write's bytes"
        );
    }

    #[test]
    fn read_rejects_virtual_path_without_leading_slash() {
        let (_dir, ctx) = temp_workspace();
        let result = ctx.read("media/whatever.png");
        assert!(matches!(result, Err(WorkspaceError::InvalidVirtualPath(_))));
    }

    #[test]
    fn read_rejects_path_traversal_attempt() {
        let (_dir, ctx) = temp_workspace();
        // cap-std 的 Dir 在操作系统层面拒绝 ".." 跳出沙盒，这里验证
        // 这个保证确实生效，而不是被我们自己的 strip_virtual_prefix
        // 逻辑意外放行。
        let result = ctx.read("/media/../../../etc/passwd");
        assert!(result.is_err(), "path traversal must fail, got: {result:?}");
    }
}

#[cfg(test)]
mod tests_files {
    use super::*;

    fn ws() -> (tempfile::TempDir, WorkspaceContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let ctx = WorkspaceContext::open_or_create(dir.path()).expect("open");
        (dir, ctx)
    }

    #[test]
    fn creates_three_subdirs_and_absolute_physical_root() {
        let (dir, ctx) = ws();
        for d in [MEDIA_DIR, EXPORTS_DIR, INPUTS_DIR] {
            assert!(ctx.list(d).expect("list").is_empty(), "{d}");
        }
        assert!(ctx.physical_root().is_absolute());
        assert_eq!(ctx.physical_root(), std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn write_keeps_name_and_dedups_with_serial() {
        let (_d, ctx) = ws();
        assert_eq!(ctx.write_file("exports", "报表.xlsx", b"1").unwrap(), "/exports/报表.xlsx");
        assert_eq!(ctx.write_file("exports", "报表.xlsx", b"2").unwrap(), "/exports/报表 (1).xlsx");
        assert_eq!(ctx.write_file("exports", "报表.xlsx", b"3").unwrap(), "/exports/报表 (2).xlsx");
        assert_eq!(ctx.read("/exports/报表.xlsx").unwrap(), b"1");
        assert_eq!(ctx.read("/exports/报表 (1).xlsx").unwrap(), b"2");
    }

    #[test]
    fn list_skips_office_lock_files() {
        let (_d, ctx) = ws();
        ctx.write_file("exports", "a.xlsx", b"x").unwrap();
        ctx.write_file("exports", ".~lock.a.xlsx#", b"lock").unwrap();
        let names: Vec<String> = ctx.list("exports").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.xlsx".to_string()], "锁文件不应出现在 list 结果里");
    }

    #[test]
    fn dedup_without_extension_and_dotfile() {
        let (_d, ctx) = ws();
        assert_eq!(ctx.write_file("inputs", "README", b"a").unwrap(), "/inputs/README");
        assert_eq!(ctx.write_file("inputs", "README", b"b").unwrap(), "/inputs/README (1)");
        assert_eq!(ctx.write_file("inputs", ".env", b"a").unwrap(), "/inputs/.env");
        assert_eq!(ctx.write_file("inputs", ".env", b"b").unwrap(), "/inputs/.env (1)");
    }

    #[test]
    fn rejects_bad_names_and_dirs() {
        let (_d, ctx) = ws();
        for name in ["", ".", "..", "a/b", "a\\b", "../x", "a\nb"] {
            assert!(
                matches!(ctx.write_file("exports", name, b"x"), Err(WorkspaceError::InvalidFileName(_))),
                "name {name:?}"
            );
        }
        for dir in ["media", "../exports", "", "exports/sub"] {
            assert!(
                matches!(ctx.write_file(dir, "a.txt", b"x"), Err(WorkspaceError::InvalidDirectory(_))),
                "dir {dir:?}"
            );
        }
        assert!(matches!(ctx.list("nope"), Err(WorkspaceError::InvalidDirectory(_))));
    }

    #[test]
    fn list_sorted_with_size_and_virtual_path() {
        let (_d, ctx) = ws();
        ctx.write_file("inputs", "b.csv", b"12345").unwrap();
        ctx.write_file("inputs", "a.csv", b"1").unwrap();
        let v = ctx.list("inputs").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].name.as_str(), v[0].size_bytes), ("a.csv", 1));
        assert_eq!(v[1].virtual_path, "/inputs/b.csv");
        assert_eq!(v[1].size_bytes, 5);
    }

    #[test]
    fn physical_path_existing_ok_missing_and_escape_err() {
        let (_d, ctx) = ws();
        let vp = ctx.write_file("exports", "out.xlsx", b"x").unwrap();
        let pp = ctx.physical_path(&vp).unwrap();
        assert_eq!(pp, ctx.physical_root().join("exports/out.xlsx"));
        assert!(pp.is_file());
        assert!(matches!(ctx.physical_path("/exports/none.xlsx"), Err(WorkspaceError::PathNotFound(_))));
        assert!(matches!(ctx.physical_path("/exports/../../etc/passwd"), Err(WorkspaceError::PathNotFound(_))));
        assert!(matches!(ctx.physical_path("exports/out.xlsx"), Err(WorkspaceError::InvalidVirtualPath(_))));
    }
}
