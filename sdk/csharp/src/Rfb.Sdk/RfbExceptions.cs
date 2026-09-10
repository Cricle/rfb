namespace Rfb.Sdk;

/// <summary>Base error for the RFB SDK (UNIFIED_API.md §7).</summary>
public class RfbException : Exception
{
    public RfbException(string message) : base(message) { }
}

/// <summary>Connection/read/write failure or timeout.</summary>
public class TransportException : RfbException
{
    public TransportException(string message) : base(message) { }
}

/// <summary>forkd controller returned a non-2xx status (carries status + message).</summary>
public class HttpStatusException : RfbException
{
    public HttpStatusException(int status, string message)
        : base($"forkd returned {status}: {message}")
    {
        Status = status;
    }

    public int Status { get; }
}

/// <summary>Response/frame decoding failed (includes strict codec rejections).</summary>
public class DecodeException : RfbException
{
    public DecodeException(string message) : base(message) { }
}

/// <summary>The remote peer reported an error (guest error line, ZBRT Error frame, forkd error field).</summary>
public class RemoteException : RfbException
{
    public RemoteException(string message) : base(message) { }
}

/// <summary>Local validation failed before sending (fail closed).</summary>
public class ValidationException : RfbException
{
    public ValidationException(string message) : base(message) { }
}
