from pathlib import Path
import platform
import shutil
import sysconfig

from setuptools import find_packages, setup
from setuptools.command.build_py import build_py
from wheel.bdist_wheel import bdist_wheel


PACKAGE_NAME = "llm_harness_sdk"


def repo_root():
    return Path(__file__).resolve().parents[3]


def native_library_name():
    system = platform.system()
    if system == "Windows":
        return "llm_harness_ffi.dll"
    if system == "Darwin":
        return "libllm_harness_ffi.dylib"
    return "libllm_harness_ffi.so"


def package_version():
    cargo_toml = repo_root() / "Cargo.toml"
    in_workspace_package = False
    for raw_line in cargo_toml.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if line.startswith("[") and line.endswith("]"):
            in_workspace_package = line == "[workspace.package]"
            continue
        if in_workspace_package and line.startswith("version"):
            return line.split("=", 1)[1].strip().strip('"')

    raise RuntimeError(f"workspace package version not found in {cargo_toml}")


class BuildPy(build_py):
    def run(self):
        super().run()
        native_src = repo_root() / "target" / "release" / native_library_name()
        if not native_src.exists():
            raise RuntimeError(
                f"native library not found at {native_src}; "
                "run `cargo build -p llm-harness-ffi --release` first"
            )

        target_dir = Path(self.build_lib) / PACKAGE_NAME / "native"
        target_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(native_src, target_dir / native_src.name)


class PlatformWheel(bdist_wheel):
    def finalize_options(self):
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self):
        platform_tag = self.plat_name or sysconfig.get_platform()
        platform_tag = platform_tag.replace("-", "_").replace(".", "_")
        return "py3", "none", platform_tag


setup(
    name="llm-harness-sdk",
    version=package_version(),
    description="Python SDK for llm-harness native runtime",
    packages=find_packages(),
    include_package_data=True,
    package_data={PACKAGE_NAME: ["native/*"]},
    python_requires=">=3.9",
    install_requires=["cffi>=1.15"],
    cmdclass={"build_py": BuildPy, "bdist_wheel": PlatformWheel},
)
