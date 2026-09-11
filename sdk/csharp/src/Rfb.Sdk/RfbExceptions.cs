namespace Rfb.Sdk;

/// <summary>Base error for the RFB SDK (UNIFIED_API.md section 7).</summary>
public class RfbException : Exception
{
    /// <summary>Create with a message.</summary>
    public RfbException(string message) : base(message) { }

    /// <summary>Create with a message and the underlying cause.</summary>
    public RfbException(string message, Exception inner) : base(message, inner) { }
}

/// <summary>Connection/read/write failure or timeout.</summary>
public class TransportException : RfbException
{
    /// <summary>Create with a message.</summary>
    public TransportException(string message) : base(message) { }

    /// <summary>Create with a message and the underlying socket/IO cause.</summary>
    public TransportException(string message, Exception inner) : base(message, inner) { }
}

/// <summary>forkd controller returned a non-2xx status (carries status + message).</summary>
public class HttpStatusException : RfbException
{
    /// <summary>Create with the HTTP status code and controller message.</summary>
    public HttpStatusException(int status, string message)
        : base($"forkd returned {status}: {message}")
    {
        Status = status;
    }

    /// <summary>HTTP status code returned by the controller.</summary>
    public int Status { get; }
}

/// <summary>Response/frame decoding failed (includes strict codec rejections).</summary>
public class DecodeException : RfbException
{
    /// <summary>Create with a message.</summary>
    public DecodeException(string message) : base(message) { }

    /// <summary>Create with a message and the underlying parse cause.</summary>
    public DecodeException(string message, Exception inner) : base(message, inner) { }
}

/// <summary>The remote peer reported an error (guest error line, ZBRT Error frame, forkd error field).</summary>
public class RemoteException : RfbException
{
    /// <summary>Create with the peer-reported message.</summary>
    public RemoteException(string message) : base(message) { }
}

/// <summary>Local validation failed before sending (fail closed).</summary>
public class ValidationException : RfbException
{
    /// <summary>Create with a message describing the rejected input.</summary>
    public ValidationException(string message) : base(message) { }
}
