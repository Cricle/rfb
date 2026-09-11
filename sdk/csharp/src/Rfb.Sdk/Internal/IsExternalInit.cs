// Polyfill: `init` accessors / record structs reference this compiler-injected
// type, which is only in-box for net8.0 (and .NET 5+). Needed for the
// netstandard2.0 / netstandard2.1 targets.
#if !NET8_0_OR_GREATER
namespace System.Runtime.CompilerServices
{
    internal static class IsExternalInit
    {
    }
}
#endif
